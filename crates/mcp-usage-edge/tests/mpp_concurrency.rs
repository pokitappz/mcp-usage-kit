//! Does the replay guard actually hold under concurrency?
//!
//! The draft requires that concurrent requests with the same credential settle
//! at most once. The committed suite only replays sequentially, which a
//! check-then-insert bug would pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mcp_usage_edge::mpp::ReplayGuard;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_proof_is_claimed_once_under_contention() {
    for round in 0..200 {
        let guard = Arc::new(ReplayGuard::new(4096, Duration::from_secs(600)));
        let wins = Arc::new(AtomicUsize::new(0));
        let key = format!("proof-{round}");

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let guard = guard.clone();
            let wins = wins.clone();
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                if guard.reserve(&key) {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for task in tasks {
            task.await.expect("task");
        }

        assert_eq!(
            wins.load(Ordering::SeqCst),
            1,
            "round {round}: a proof must be reservable exactly once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn distinct_proofs_all_succeed_under_contention() {
    let guard = Arc::new(ReplayGuard::new(4096, Duration::from_secs(600)));
    let wins = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for index in 0..500 {
        let guard = guard.clone();
        let wins = wins.clone();
        tasks.push(tokio::spawn(async move {
            if guard.reserve(&format!("distinct-{index}")) {
                wins.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }
    assert_eq!(wins.load(Ordering::SeqCst), 500);
}
