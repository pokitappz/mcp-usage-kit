//! One customer the provider refuses must not stop every other customer's
//! usage from being invoiced.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use mcp_usage_export::{
    AggregatedUsage, BatchExporter, BillingPipeline, ExportError, ExportFuture, UsageEvent,
    UsageRecorder,
};

#[derive(Default)]
struct Provider {
    /// Refuse any batch touching this customer.
    poison: Option<&'static str>,
    /// Refuse everything, for an outage.
    down: AtomicBool,
    accepted: Mutex<Vec<(String, u64)>>,
}

impl Provider {
    fn refusing(poison: &'static str) -> Self {
        Self {
            poison: Some(poison),
            ..Self::default()
        }
    }

    fn units_for(&self, customer: &str) -> u64 {
        self.accepted
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| c == customer)
            .map(|(_, units)| units)
            .sum()
    }
}

impl BatchExporter for Provider {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            let refused = self.down.load(Ordering::SeqCst)
                || self
                    .poison
                    .is_some_and(|poison| batch.iter().any(|u| u.customer_id == poison));
            if refused {
                return Err(ExportError::Provider("refused".to_owned()));
            }
            let mut accepted = self.accepted.lock().unwrap();
            for usage in batch {
                accepted.push((usage.customer_id.clone(), usage.units));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn a_batch_the_provider_will_never_accept_does_not_block_anyone_else() {
    // Without this, one bad batch is re-offered on every flush forever and no
    // other customer's usage is ever exported again. The buffer is in memory,
    // so on the next deploy all of it is simply gone.
    let pipeline = BillingPipeline::new(Provider::refusing("cus_poison"));
    pipeline
        .record(UsageEvent::now("t", "cus_poison", "m", 1000, None))
        .unwrap();

    for _ in 0..20 {
        pipeline
            .record(UsageEvent::now("t", "cus_healthy", "m", 5, None))
            .unwrap();
        let _ = pipeline.flush().await;
    }

    assert!(
        pipeline.exporter().units_for("cus_healthy") > 0,
        "healthy usage must still reach the provider"
    );
    assert_eq!(
        pipeline.exporter().units_for("cus_poison"),
        0,
        "and the refused batch must not be invented as accepted"
    );
    // The stuck batch stays visible rather than being silently dropped, and
    // it does not grow: every later flush that fails restores the same
    // aggregates rather than accumulating new ones behind them.
    let stuck = pipeline.retry_buckets();
    assert!(stuck > 0, "the refused batch must still be owed");
    for _ in 0..20 {
        pipeline
            .record(UsageEvent::now("t", "cus_healthy", "m", 5, None))
            .unwrap();
        let _ = pipeline.flush().await;
    }
    assert_eq!(
        pipeline.retry_buckets(),
        stuck,
        "the retry queue must not grow while it is stuck"
    );
}

#[tokio::test]
async fn a_refused_batch_holds_only_the_aggregates_it_was_batched_with() {
    // Worth stating plainly: aggregates are flushed as one batch, so a
    // customer the provider refuses does hold hostage whatever happened to be
    // in the same batch. What it must not do is hold up everything recorded
    // afterwards, which is the difference between a delayed invoice and a
    // stopped one.
    let pipeline = BillingPipeline::new(Provider::refusing("cus_poison"));
    pipeline
        .record(UsageEvent::now("t", "cus_poison", "m", 1, None))
        .unwrap();
    pipeline
        .record(UsageEvent::now("t", "cus_batched_with_it", "m", 2, None))
        .unwrap();
    let _ = pipeline.flush().await;
    assert_eq!(pipeline.retry_buckets(), 2, "both went out together");

    // Everything recorded after the failure still gets through.
    for _ in 0..10 {
        pipeline
            .record(UsageEvent::now("t", "cus_later", "m", 3, None))
            .unwrap();
        let _ = pipeline.flush().await;
    }
    assert!(pipeline.exporter().units_for("cus_later") > 0);
}

#[tokio::test]
async fn a_transient_outage_loses_nothing_once_the_provider_returns() {
    // The alternation must not turn a retry into a drop: the whole point of
    // keeping identifiers is that the batch is still owed.
    let pipeline = BillingPipeline::new(Provider::default());
    pipeline.exporter().down.store(true, Ordering::SeqCst);

    pipeline
        .record(UsageEvent::now("t", "cus_a", "m", 7, None))
        .unwrap();
    for _ in 0..5 {
        pipeline
            .record(UsageEvent::now("t", "cus_b", "m", 1, None))
            .unwrap();
        let _ = pipeline.flush().await;
    }
    assert_eq!(pipeline.exporter().accepted.lock().unwrap().len(), 0);

    pipeline.exporter().down.store(false, Ordering::SeqCst);
    for _ in 0..10 {
        let _ = pipeline.flush().await;
    }

    assert_eq!(
        pipeline.exporter().units_for("cus_a"),
        7,
        "usage buffered during the outage is still owed and must arrive"
    );
    assert_eq!(pipeline.exporter().units_for("cus_b"), 5);
    assert_eq!(pipeline.retry_buckets(), 0);
    assert_eq!(pipeline.pending_buckets(), 0);
}

#[tokio::test]
async fn a_retry_is_not_starved_when_there_is_no_fresh_usage() {
    // Alternation only applies when there is something to alternate with.
    let pipeline = BillingPipeline::new(Provider::default());
    pipeline.exporter().down.store(true, Ordering::SeqCst);
    pipeline
        .record(UsageEvent::now("t", "cus_a", "m", 3, None))
        .unwrap();
    assert!(pipeline.flush().await.is_err());
    assert_eq!(pipeline.retry_buckets(), 1);

    pipeline.exporter().down.store(false, Ordering::SeqCst);
    // One flush, no fresh usage: the retry must be taken immediately.
    assert_eq!(pipeline.flush().await.unwrap(), 1);
    assert_eq!(pipeline.exporter().units_for("cus_a"), 3);
}

#[tokio::test]
async fn retry_and_pending_are_reported_separately() {
    // "pending is high" and "the provider is refusing" are different
    // incidents, and one counter cannot tell an operator which is happening.
    let pipeline = BillingPipeline::new(Provider::default());
    pipeline.exporter().down.store(true, Ordering::SeqCst);
    pipeline
        .record(UsageEvent::now("t", "cus_a", "m", 1, None))
        .unwrap();
    assert!(pipeline.flush().await.is_err());

    pipeline
        .record(UsageEvent::now("t", "cus_b", "m", 1, None))
        .unwrap();

    assert_eq!(pipeline.retry_buckets(), 1);
    assert_eq!(
        pipeline.pending_buckets(),
        2,
        "documented as pending + retry"
    );
}
