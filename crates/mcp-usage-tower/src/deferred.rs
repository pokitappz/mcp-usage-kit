//! Terminal accounting that could not finish without awaiting.
//!
//! Accounting runs when the response body is released, and a body is released
//! from `Drop`, where nothing may await. That is fine for synchronous recorders
//! and for the in-process task store, whose futures never yield. A durable store
//! backed by Redis or `PostgreSQL` does real I/O, so its first poll pends, and
//! the work has nowhere to run.
//!
//! Dropping it there is not an option. The affected paths are the ones that
//! carry durable-task attribution: the `insert` that records which call
//! commissioned a task, and the `claim` that prices its completion. Losing them
//! loses the charge for every durable task, in exactly the horizontally scaled
//! deployment those stores exist for.
//!
//! So the unfinished future is parked here instead, and driven later from a
//! context that can await. Two things drive it, deliberately: every subsequent
//! request drains a bounded number, so an application that does nothing still
//! converges, and [`DeferredCompletions::drain`] empties the queue on demand for
//! timeliness and for shutdown.
//!
//! Parking never allocates a runtime and never spawns, which is what keeps this
//! crate runtime agnostic.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A parked terminal-accounting future.
type Parked = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Queue of terminal accounting waiting for somewhere to await.
///
/// Obtain one from [`EdgeConfig::deferred`](crate::EdgeConfig::deferred) before
/// handing the configuration to the layer, the same way edge metrics are
/// obtained.
pub struct DeferredCompletions {
    queue: Mutex<VecDeque<Parked>>,
    capacity: usize,
    dropped: AtomicU64,
}

impl std::fmt::Debug for DeferredCompletions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeferredCompletions")
            .field("pending", &self.len())
            .field("capacity", &self.capacity)
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DeferredCompletions {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            capacity,
            dropped: AtomicU64::new(0),
        }
    }

    /// Park an unfinished completion, reporting whether it was accepted.
    ///
    /// The queue is bounded, because an application that never drains must not
    /// be able to grow it without limit. At capacity the *oldest* entry is
    /// evicted: the newest accounting is the most likely to still be
    /// claimable, and an unbounded backlog is worse than a bounded one.
    pub(crate) fn park(&self, completion: Parked) -> bool {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.capacity == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let mut evicted = false;
        while queue.len() >= self.capacity {
            queue.pop_front();
            evicted = true;
        }
        queue.push_back(completion);
        if evicted {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        !evicted
    }

    fn take(&self, limit: usize) -> Vec<Parked> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = limit.min(queue.len());
        queue.drain(..count).collect()
    }

    /// Run at most `limit` parked completions, returning how many ran.
    ///
    /// Entries are removed from the queue before being awaited, so a caller that
    /// is cancelled part way through drops the completions it had taken rather
    /// than leaving them to be run twice.
    pub async fn drain_some(&self, limit: usize) -> usize {
        let taken = self.take(limit);
        let count = taken.len();
        for completion in taken {
            completion.await;
        }
        count
    }

    /// Run every parked completion, returning how many ran.
    ///
    /// Call this from the same loop that flushes billing, and once more on
    /// shutdown, so durable-task accounting is not left behind by a process that
    /// is about to exit.
    pub async fn drain(&self) -> usize {
        let mut total = 0;
        loop {
            let ran = self.drain_some(self.capacity.max(1)).await;
            if ran == 0 {
                return total;
            }
            total += ran;
        }
    }

    /// Completions currently waiting to run.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Completions discarded because the queue was full.
    ///
    /// A nonzero value means usage was lost, and that draining is not keeping up
    /// with the rate at which durable-task accounting is being parked.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn counting(counter: &Arc<AtomicUsize>) -> Parked {
        let counter = Arc::clone(counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::Relaxed);
        })
    }

    #[tokio::test]
    async fn parked_completions_run_when_drained() {
        let ran = Arc::new(AtomicUsize::new(0));
        let deferred = DeferredCompletions::new(8);
        for _ in 0..3 {
            assert!(deferred.park(counting(&ran)));
        }
        assert_eq!(deferred.len(), 3);

        assert_eq!(deferred.drain().await, 3);
        assert_eq!(ran.load(Ordering::Relaxed), 3);
        assert!(deferred.is_empty());
        assert_eq!(deferred.dropped(), 0);
    }

    #[tokio::test]
    async fn draining_is_bounded_by_the_limit() {
        let ran = Arc::new(AtomicUsize::new(0));
        let deferred = DeferredCompletions::new(8);
        for _ in 0..5 {
            deferred.park(counting(&ran));
        }

        assert_eq!(deferred.drain_some(2).await, 2);
        assert_eq!(ran.load(Ordering::Relaxed), 2);
        assert_eq!(deferred.len(), 3, "the rest must stay queued");
    }

    #[tokio::test]
    async fn a_full_queue_evicts_the_oldest_and_reports_the_loss() {
        let ran = Arc::new(AtomicUsize::new(0));
        let deferred = DeferredCompletions::new(2);
        assert!(deferred.park(counting(&ran)));
        assert!(deferred.park(counting(&ran)));
        assert!(
            !deferred.park(counting(&ran)),
            "the third must report that something was discarded"
        );

        assert_eq!(deferred.len(), 2, "the queue must stay bounded");
        assert_eq!(deferred.dropped(), 1);
        assert_eq!(deferred.drain().await, 2);
    }

    #[tokio::test]
    async fn a_zero_capacity_queue_accepts_nothing() {
        let ran = Arc::new(AtomicUsize::new(0));
        let deferred = DeferredCompletions::new(0);
        assert!(!deferred.park(counting(&ran)));
        assert!(deferred.is_empty());
        assert_eq!(deferred.dropped(), 1);
        assert_eq!(deferred.drain().await, 0);
        assert_eq!(ran.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn draining_an_empty_queue_is_a_no_op() {
        let deferred = DeferredCompletions::new(4);
        assert_eq!(deferred.drain().await, 0);
        assert_eq!(deferred.drain_some(10).await, 0);
    }
}
