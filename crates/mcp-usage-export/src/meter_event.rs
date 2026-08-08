//! Provider-neutral meter event export with partial-batch retry progress.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use thiserror::Error;

use super::{AggregatedUsage, BatchExporter, ExportError, ExportFuture};

const DEFAULT_DEAD_LETTER_CAPACITY: usize = 1_024;

/// Boxed future returned by a meter event provider.
pub type MeterEventProviderFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<MeterEventOutcome>, MeterEventProviderError>> + Send + 'a>,
>;

/// Submits ordered batches of pre-aggregated usage to a billing provider.
///
/// Implementations must return exactly one ordered [`MeterEventOutcome`] for
/// every submitted aggregate. They must treat [`AggregatedUsage::identifier`]
/// as an idempotency key because cancellation and batch-wide failures can leave
/// an accepted event's result ambiguous.
pub trait MeterEventProvider: Send + Sync {
    /// Submit a batch and return one outcome for each aggregate in the same order.
    ///
    /// A batch-wide error means no per-event outcome is available. Error and
    /// outcome codes must be static, low-cardinality categories and must not
    /// contain credentials, customer data, or other identifying values.
    fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a>;
}

/// The provider's final or retryable disposition for one meter event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterEventOutcome {
    /// The provider accepted the event.
    Accepted,
    /// The event remains unresolved and must be retried with the same identifier.
    RetryableFailure {
        /// Static, sanitized, low-cardinality failure category.
        code: &'static str,
    },
    /// The provider permanently rejected this individual event.
    PermanentRejection {
        /// Static, sanitized, low-cardinality rejection category.
        code: &'static str,
    },
}

/// Sanitized batch-wide provider failure without per-event outcomes.
///
/// The code is restricted to static data to discourage embedding runtime
/// credentials or customer values. Provider implementations must use a small,
/// bounded set of categories such as `unavailable` or `authentication_failed`.
#[derive(Clone, Copy, PartialEq, Eq, Error)]
#[error("meter event provider failed: {code}")]
pub struct MeterEventProviderError {
    code: &'static str,
}

impl MeterEventProviderError {
    /// Construct an error from a static, sanitized, low-cardinality code.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// Return the sanitized failure code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Debug for MeterEventProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MeterEventProviderError")
            .field("code", &self.code)
            .finish()
    }
}

/// A meter event that needs application-owned reconciliation.
#[derive(Clone, PartialEq, Eq)]
pub struct MeterEventDeadLetter {
    /// The original aggregate, including its stable identifier and timestamp.
    pub aggregate: AggregatedUsage,
    /// Why the aggregate was quarantined.
    pub reason: MeterEventDeadLetterReason,
}

impl fmt::Debug for MeterEventDeadLetter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MeterEventDeadLetter")
            .field("aggregate", &"[REDACTED]")
            .field("reason", &self.reason)
            .finish()
    }
}

/// Why a meter event was quarantined for reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterEventDeadLetterReason {
    /// The provider synchronously and permanently rejected the event.
    SynchronousProviderRejection {
        /// Static, sanitized, low-cardinality rejection category.
        code: &'static str,
    },
    /// The provider reported a rejection after initially accepting the event.
    AsynchronousProviderRejection {
        /// Static, sanitized, low-cardinality rejection category.
        code: &'static str,
    },
}

#[derive(Default)]
struct ExporterState {
    dead_letters: VecDeque<MeterEventDeadLetter>,
    dead_letter_identifiers: HashSet<String>,
    dropped_dead_letters: u64,
    retry_progress: Option<RetryProgress>,
}

struct RetryProgress {
    batch: Vec<AggregatedUsage>,
    completed: Vec<bool>,
}

struct ExportLease<'a> {
    exporting: &'a AtomicBool,
}

impl Drop for ExportLease<'_> {
    fn drop(&mut self) {
        self.exporting.store(false, Ordering::Release);
    }
}

/// Exports pre-aggregated usage through an application-supplied provider.
///
/// Confirmed accepted and permanently rejected events are skipped on a later
/// retry. Retryable and ambiguous events retain their original identifiers.
/// While partial progress exists, only the identical original batch is accepted.
pub struct MeterEventExporter<P> {
    provider: P,
    dead_letter_capacity: usize,
    state: Mutex<ExporterState>,
    exporting: AtomicBool,
}

impl<P> MeterEventExporter<P> {
    /// Construct an exporter with a dead letter capacity of 1,024.
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            dead_letter_capacity: DEFAULT_DEAD_LETTER_CAPACITY,
            state: Mutex::new(ExporterState::default()),
            exporting: AtomicBool::new(false),
        }
    }

    /// Access the provider for diagnostics and tests.
    #[must_use]
    pub const fn provider(&self) -> &P {
        &self.provider
    }

    /// Bound how many rejected aggregates are retained for reconciliation.
    ///
    /// Reducing the capacity evicts the oldest entries and increments
    /// [`MeterEventExporter::dropped_dead_letters`]. Zero disables retention
    /// while continuing to count discarded entries.
    #[must_use]
    pub fn with_dead_letter_capacity(mut self, capacity: usize) -> Self {
        self.dead_letter_capacity = capacity;
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.dead_letters.len() > capacity {
            if let Some(evicted) = state.dead_letters.pop_front() {
                state
                    .dead_letter_identifiers
                    .remove(&evicted.aggregate.identifier);
                state.dropped_dead_letters = state.dropped_dead_letters.saturating_add(1);
            }
        }
        self
    }

    /// Number of aggregates retained for reconciliation.
    #[must_use]
    pub fn dead_letter_count(&self) -> usize {
        self.lock_state().dead_letters.len()
    }

    /// Number of reconciliation aggregates discarded because retention was full.
    #[must_use]
    pub fn dropped_dead_letters(&self) -> u64 {
        self.lock_state().dropped_dead_letters
    }

    /// Drain retained aggregates for application-owned reconciliation.
    #[must_use]
    pub fn take_dead_letters(&self) -> Vec<MeterEventDeadLetter> {
        let mut state = self.lock_state();
        state.dead_letter_identifiers.clear();
        state.dead_letters.drain(..).collect()
    }

    /// Retain an aggregate that a provider rejected after initial acceptance.
    ///
    /// Returns `true` when the aggregate was newly retained. A duplicate
    /// identifier or zero-capacity queue returns `false`. The code must be a
    /// static, sanitized, low-cardinality category.
    #[must_use]
    pub fn quarantine_async_rejection(
        &self,
        aggregate: &AggregatedUsage,
        code: &'static str,
    ) -> bool {
        let mut state = self.lock_state();
        self.quarantine_locked(
            &mut state,
            aggregate,
            MeterEventDeadLetterReason::AsynchronousProviderRejection { code },
        )
    }

    /// Whether an incomplete batch is still owed a retry.
    ///
    /// While this is true, [`BatchExporter::export`] accepts only the identical
    /// original batch and refuses any other.
    #[must_use]
    pub fn retry_in_progress(&self) -> bool {
        self.lock_state().retry_progress.is_some()
    }

    /// Give up on an incomplete batch so a different one can be exported.
    ///
    /// Returns the identifiers that were not confirmed complete. If an export is
    /// currently running, no progress is abandoned and an empty vector is
    /// returned. Callers can distinguish that case with [`Self::retry_in_progress`].
    #[must_use]
    pub fn abandon_retry_progress(&self) -> Vec<String> {
        let Ok(_lease) = self.acquire_lease() else {
            return Vec::new();
        };
        let Some(progress) = self.lock_state().retry_progress.take() else {
            return Vec::new();
        };
        progress
            .batch
            .into_iter()
            .zip(progress.completed)
            .filter_map(|(aggregate, completed)| (!completed).then_some(aggregate.identifier))
            .collect()
    }

    fn lock_state(&self) -> MutexGuard<'_, ExporterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn acquire_lease(&self) -> Result<ExportLease<'_>, ExportError> {
        self.exporting
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| {
                ExportError::Provider("meter event export already in progress".to_owned())
            })?;
        Ok(ExportLease {
            exporting: &self.exporting,
        })
    }

    fn begin_export(&self, batch: &[AggregatedUsage]) -> Result<ExportLease<'_>, ExportError> {
        let lease = self.acquire_lease()?;
        let mut state = self.lock_state();
        match &state.retry_progress {
            Some(progress) if progress.batch != batch => {
                return Err(ExportError::Provider(
                    "meter event exporter requires the incomplete batch to be retried first; \
                     call abandon_retry_progress to give up on it"
                        .to_owned(),
                ));
            }
            Some(_) => {}
            None => {
                state.retry_progress = Some(RetryProgress {
                    batch: batch.to_vec(),
                    completed: vec![false; batch.len()],
                });
            }
        }
        drop(state);
        Ok(lease)
    }

    fn unresolved(&self) -> Vec<(usize, AggregatedUsage)> {
        let state = self.lock_state();
        let Some(progress) = &state.retry_progress else {
            return Vec::new();
        };
        progress
            .batch
            .iter()
            .enumerate()
            .filter(|(index, _)| !progress.completed[*index])
            .map(|(index, aggregate)| (index, aggregate.clone()))
            .collect()
    }

    fn apply_outcomes(
        &self,
        unresolved: &[(usize, AggregatedUsage)],
        outcomes: Vec<MeterEventOutcome>,
    ) -> bool {
        let mut state = self.lock_state();
        let mut has_retryable = false;
        for ((index, aggregate), outcome) in unresolved.iter().zip(outcomes) {
            match outcome {
                MeterEventOutcome::Accepted => {
                    Self::mark_completed_locked(&mut state, *index);
                }
                MeterEventOutcome::RetryableFailure { .. } => {
                    has_retryable = true;
                }
                MeterEventOutcome::PermanentRejection { code } => {
                    self.quarantine_locked(
                        &mut state,
                        aggregate,
                        MeterEventDeadLetterReason::SynchronousProviderRejection { code },
                    );
                    Self::mark_completed_locked(&mut state, *index);
                }
            }
        }
        has_retryable
    }

    fn mark_completed_locked(state: &mut ExporterState, index: usize) {
        if let Some(completed) = state
            .retry_progress
            .as_mut()
            .and_then(|progress| progress.completed.get_mut(index))
        {
            *completed = true;
        }
    }

    fn quarantine_locked(
        &self,
        state: &mut ExporterState,
        aggregate: &AggregatedUsage,
        reason: MeterEventDeadLetterReason,
    ) -> bool {
        if state
            .dead_letter_identifiers
            .contains(&aggregate.identifier)
        {
            return false;
        }
        if self.dead_letter_capacity == 0 {
            state.dropped_dead_letters = state.dropped_dead_letters.saturating_add(1);
            return false;
        }
        while state.dead_letters.len() >= self.dead_letter_capacity {
            if let Some(evicted) = state.dead_letters.pop_front() {
                state
                    .dead_letter_identifiers
                    .remove(&evicted.aggregate.identifier);
                state.dropped_dead_letters = state.dropped_dead_letters.saturating_add(1);
            }
        }
        state
            .dead_letter_identifiers
            .insert(aggregate.identifier.clone());
        state.dead_letters.push_back(MeterEventDeadLetter {
            aggregate: aggregate.clone(),
            reason,
        });
        true
    }

    fn commit_export(&self) {
        self.lock_state().retry_progress = None;
    }
}

impl<P> fmt::Debug for MeterEventExporter<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MeterEventExporter")
            .field("provider", &"[REDACTED]")
            .field("dead_letters", &self.dead_letter_count())
            .field("dropped_dead_letters", &self.dropped_dead_letters())
            .field("dead_letter_capacity", &self.dead_letter_capacity)
            .field(
                "export_in_progress",
                &self.exporting.load(Ordering::Relaxed),
            )
            .field("retry_in_progress", &self.retry_in_progress())
            .finish_non_exhaustive()
    }
}

impl<P: MeterEventProvider> BatchExporter for MeterEventExporter<P> {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            let _lease = self.begin_export(batch)?;
            let unresolved = self.unresolved();
            if unresolved.is_empty() {
                self.commit_export();
                return Ok(());
            }
            let submitted = unresolved
                .iter()
                .map(|(_, aggregate)| aggregate.clone())
                .collect::<Vec<_>>();
            let outcomes = self
                .provider
                .submit(&submitted)
                .await
                .map_err(|error| ExportError::Provider(error.to_string()))?;
            if outcomes.len() != unresolved.len() {
                return Err(ExportError::Provider(
                    "meter event provider returned an invalid outcome count".to_owned(),
                ));
            }
            if self.apply_outcomes(&unresolved, outcomes) {
                return Err(ExportError::Provider(
                    "meter event provider reported retryable events".to_owned(),
                ));
            }
            self.commit_export();
            Ok(())
        })
    }
}
