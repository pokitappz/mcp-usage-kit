use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mcp_usage_export::{
    AggregatedUsage, BatchExporter, BillingPipeline, MeterEventDeadLetterReason,
    MeterEventExporter, MeterEventOutcome, MeterEventProvider, MeterEventProviderError,
    MeterEventProviderFuture, UsageEvent, UsageRecorder,
};

type ProviderResult = Result<Vec<MeterEventOutcome>, MeterEventProviderError>;

#[derive(Default)]
struct FakeProvider {
    attempts: Mutex<Vec<Vec<AggregatedUsage>>>,
    responses: Mutex<VecDeque<ProviderResult>>,
}

impl FakeProvider {
    fn with_responses(responses: impl IntoIterator<Item = ProviderResult>) -> Self {
        Self {
            attempts: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn attempts(&self) -> Vec<Vec<AggregatedUsage>> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl MeterEventProvider for FakeProvider {
    fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a> {
        Box::pin(async move {
            self.attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(batch.to_vec());
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| Ok(vec![MeterEventOutcome::Accepted; batch.len()]))
        })
    }
}

fn aggregate(identifier: &str) -> AggregatedUsage {
    AggregatedUsage {
        identifier: identifier.to_owned(),
        customer_id: format!("customer-{identifier}"),
        meter: format!("meter-{identifier}"),
        units: 42,
        timestamp: 1_800_000_000,
    }
}

#[tokio::test]
async fn accepted_batch_completes_without_retry_progress() {
    let provider = FakeProvider::with_responses([Ok(vec![
        MeterEventOutcome::Accepted,
        MeterEventOutcome::Accepted,
    ])]);
    let exporter = MeterEventExporter::new(provider);
    let batch = [aggregate("one"), aggregate("two")];

    exporter.export(&batch).await.unwrap();

    assert!(!exporter.retry_in_progress());
    assert_eq!(exporter.provider().attempts(), vec![batch.to_vec()]);
}

#[tokio::test]
async fn mixed_outcomes_skip_completed_events_on_retry() {
    let provider = FakeProvider::with_responses([
        Ok(vec![
            MeterEventOutcome::Accepted,
            MeterEventOutcome::PermanentRejection { code: "invalid" },
            MeterEventOutcome::RetryableFailure { code: "busy" },
        ]),
        Ok(vec![MeterEventOutcome::Accepted]),
    ]);
    let exporter = MeterEventExporter::new(provider);
    let batch = [
        aggregate("accepted"),
        aggregate("rejected"),
        aggregate("retryable"),
    ];

    assert!(exporter.export(&batch).await.is_err());
    assert!(exporter.retry_in_progress());
    exporter.export(&batch).await.unwrap();

    let attempts = exporter.provider().attempts();
    assert_eq!(attempts[0], batch);
    assert_eq!(attempts[1], vec![batch[2].clone()]);
    let dead_letters = exporter.take_dead_letters();
    assert_eq!(dead_letters.len(), 1);
    assert_eq!(dead_letters[0].aggregate, batch[1]);
    assert_eq!(
        dead_letters[0].reason,
        MeterEventDeadLetterReason::SynchronousProviderRejection { code: "invalid" }
    );
}

#[tokio::test]
async fn billing_pipeline_retains_original_batch_while_provider_gets_only_unresolved_events() {
    let provider = FakeProvider::with_responses([
        Ok(vec![
            MeterEventOutcome::Accepted,
            MeterEventOutcome::RetryableFailure { code: "busy" },
        ]),
        Ok(vec![MeterEventOutcome::Accepted]),
    ]);
    let pipeline = BillingPipeline::new(MeterEventExporter::new(provider));
    for customer in ["a", "b"] {
        pipeline
            .record(UsageEvent {
                tenant_id: customer.to_owned(),
                customer_id: customer.to_owned(),
                meter: "units".to_owned(),
                units: 1,
                idempotency_key: None,
                timestamp: 1_800_000_000,
            })
            .unwrap();
    }

    assert!(pipeline.flush().await.is_err());
    assert_eq!(pipeline.pending_buckets(), 2);
    assert_eq!(pipeline.flush().await.unwrap(), 2);
    assert_eq!(pipeline.pending_buckets(), 0);

    let attempts = pipeline.exporter().provider().attempts();
    assert_eq!(attempts[0].len(), 2);
    assert_eq!(attempts[1], vec![attempts[0][1].clone()]);
    assert_eq!(attempts[0][1].identifier, attempts[1][0].identifier);
}

#[tokio::test]
async fn batch_failure_retries_every_event_with_stable_identifiers() {
    let provider = FakeProvider::with_responses([
        Err(MeterEventProviderError::new("unavailable")),
        Ok(vec![
            MeterEventOutcome::Accepted,
            MeterEventOutcome::Accepted,
        ]),
    ]);
    let exporter = MeterEventExporter::new(provider);
    let batch = [aggregate("one"), aggregate("two")];

    assert!(exporter.export(&batch).await.is_err());
    exporter.export(&batch).await.unwrap();

    assert_eq!(exporter.provider().attempts(), vec![batch.to_vec(); 2]);
}

#[tokio::test]
async fn malformed_outcome_count_applies_nothing() {
    let provider = FakeProvider::with_responses([
        Ok(vec![MeterEventOutcome::PermanentRejection {
            code: "invalid",
        }]),
        Ok(vec![
            MeterEventOutcome::Accepted,
            MeterEventOutcome::Accepted,
        ]),
    ]);
    let exporter = MeterEventExporter::new(provider);
    let batch = [aggregate("one"), aggregate("two")];

    assert!(exporter.export(&batch).await.is_err());
    assert_eq!(exporter.dead_letter_count(), 0);
    exporter.export(&batch).await.unwrap();

    assert_eq!(exporter.provider().attempts(), vec![batch.to_vec(); 2]);
}

#[tokio::test]
async fn different_batch_is_refused_until_progress_is_abandoned() {
    let provider = FakeProvider::with_responses([
        Ok(vec![
            MeterEventOutcome::Accepted,
            MeterEventOutcome::RetryableFailure { code: "busy" },
        ]),
        Ok(vec![MeterEventOutcome::Accepted]),
    ]);
    let exporter = MeterEventExporter::new(provider);
    let first = [aggregate("complete"), aggregate("unresolved")];
    let different = [aggregate("different")];

    assert!(exporter.export(&first).await.is_err());
    let error = exporter.export(&different).await.unwrap_err();
    assert!(format!("{error}").contains("abandon_retry_progress"));
    assert_eq!(exporter.provider().attempts().len(), 1);
    assert_eq!(exporter.abandon_retry_progress(), vec!["unresolved"]);
    assert!(!exporter.retry_in_progress());
    exporter.export(&different).await.unwrap();
}

#[tokio::test]
async fn full_batch_identity_is_required_for_retry() {
    let provider = FakeProvider::with_responses([Ok(vec![MeterEventOutcome::RetryableFailure {
        code: "busy",
    }])]);
    let exporter = MeterEventExporter::new(provider);
    let original = [aggregate("same-id")];
    let mut changed = original.clone();
    changed[0].units += 1;

    assert!(exporter.export(&original).await.is_err());
    assert!(exporter.export(&changed).await.is_err());
    assert_eq!(exporter.provider().attempts().len(), 1);
}

#[tokio::test]
async fn dead_letters_are_deduplicated_bounded_and_drained() {
    let provider = FakeProvider::with_responses([Ok(vec![
        MeterEventOutcome::PermanentRejection { code: "invalid" },
        MeterEventOutcome::PermanentRejection { code: "invalid" },
        MeterEventOutcome::PermanentRejection { code: "invalid" },
    ])]);
    let exporter = MeterEventExporter::new(provider).with_dead_letter_capacity(1);
    let duplicate = aggregate("duplicate");
    let newest = aggregate("newest");

    exporter
        .export(&[duplicate.clone(), duplicate, newest.clone()])
        .await
        .unwrap();

    assert_eq!(exporter.dead_letter_count(), 1);
    assert_eq!(exporter.dropped_dead_letters(), 1);
    assert_eq!(exporter.take_dead_letters()[0].aggregate, newest);
    assert!(exporter.take_dead_letters().is_empty());
}

#[tokio::test]
async fn zero_capacity_counts_sync_and_async_discards() {
    let provider =
        FakeProvider::with_responses([Ok(vec![MeterEventOutcome::PermanentRejection {
            code: "invalid",
        }])]);
    let exporter = MeterEventExporter::new(provider).with_dead_letter_capacity(0);
    let sync_rejected = aggregate("sync");
    let async_rejected = aggregate("async");

    exporter.export(&[sync_rejected]).await.unwrap();
    assert!(!exporter.quarantine_async_rejection(&async_rejected, "late_invalid"));
    assert_eq!(exporter.dead_letter_count(), 0);
    assert_eq!(exporter.dropped_dead_letters(), 2);
}

#[test]
fn asynchronous_rejections_are_bounded_and_deduplicated() {
    let exporter = MeterEventExporter::new(FakeProvider::default()).with_dead_letter_capacity(1);
    let first = aggregate("first");
    let second = aggregate("second");

    assert!(exporter.quarantine_async_rejection(&first, "late_invalid"));
    assert!(!exporter.quarantine_async_rejection(&first, "late_invalid"));
    assert!(exporter.quarantine_async_rejection(&second, "late_invalid"));
    assert_eq!(exporter.dropped_dead_letters(), 1);
    let retained = exporter.take_dead_letters();
    assert_eq!(retained[0].aggregate, second);
    assert_eq!(
        retained[0].reason,
        MeterEventDeadLetterReason::AsynchronousProviderRejection {
            code: "late_invalid"
        }
    );
}

#[test]
fn debug_output_redacts_provider_and_aggregate_data() {
    struct SensitiveProvider(&'static str);

    impl MeterEventProvider for SensitiveProvider {
        fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a> {
            Box::pin(async move { Ok(vec![MeterEventOutcome::Accepted; batch.len()]) })
        }
    }

    let exporter = MeterEventExporter::new(SensitiveProvider("provider-secret"));
    let private = aggregate("private-identifier");
    assert!(exporter.quarantine_async_rejection(&private, "late_invalid"));

    let debug = format!("{exporter:?}");
    for private in [
        "provider-secret",
        "private-identifier",
        "customer-private-identifier",
        "meter-private-identifier",
    ] {
        assert!(!debug.contains(private));
    }
    let dead_letter_debug = format!("{:?}", exporter.take_dead_letters()[0]);
    assert!(!dead_letter_debug.contains("private-identifier"));
    assert_eq!(exporter.provider().0, "provider-secret");
}

#[derive(Default)]
struct CancelOnceProvider {
    attempts: AtomicUsize,
}

impl MeterEventProvider for CancelOnceProvider {
    fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a> {
        Box::pin(async move {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending::<()>().await;
            }
            Ok(vec![MeterEventOutcome::Accepted; batch.len()])
        })
    }
}

#[tokio::test]
async fn cancellation_leaves_all_events_retryable() {
    let exporter = Arc::new(MeterEventExporter::new(CancelOnceProvider::default()));
    let batch = vec![aggregate("cancelled")];
    let task = tokio::spawn({
        let exporter = Arc::clone(&exporter);
        let batch = batch.clone();
        async move { exporter.export(&batch).await }
    });
    while exporter.provider().attempts.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert!(exporter.export(&batch).await.is_err());
    assert!(exporter.abandon_retry_progress().is_empty());
    assert!(exporter.retry_in_progress());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert!(exporter.retry_in_progress());
    exporter.export(&batch).await.unwrap();
    assert_eq!(exporter.provider().attempts.load(Ordering::SeqCst), 2);
}

#[derive(Default)]
struct PanicOnceProvider {
    panicked: AtomicBool,
    attempts: AtomicUsize,
}

impl MeterEventProvider for PanicOnceProvider {
    fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            assert!(
                self.panicked.swap(true, Ordering::SeqCst),
                "intentional provider panic"
            );
            Ok(vec![MeterEventOutcome::Accepted; batch.len()])
        })
    }
}

#[tokio::test]
async fn panic_leaves_all_events_retryable() {
    let exporter = Arc::new(MeterEventExporter::new(PanicOnceProvider::default()));
    let batch = vec![aggregate("panicked")];
    let task = tokio::spawn({
        let exporter = Arc::clone(&exporter);
        let batch = batch.clone();
        async move { exporter.export(&batch).await }
    });
    assert!(task.await.unwrap_err().is_panic());

    assert!(exporter.retry_in_progress());
    exporter.export(&batch).await.unwrap();
    assert_eq!(exporter.provider().attempts.load(Ordering::SeqCst), 2);
}
