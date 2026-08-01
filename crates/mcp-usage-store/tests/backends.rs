//! Integration tests against real Redis and `PostgreSQL` backends.
//!
//! Both stores promise the same contract, so the invariants below are written
//! once and run against each backend. What is being checked is not "does the
//! query work" but the three properties billing correctness rests on: a task
//! origin is immutable once captured, a completed task can be claimed exactly
//! once even under contention, and records for one tenant are invisible to
//! another.
//!
//! Each backend is skipped unless its URL is present in the environment, so
//! `cargo test` stays green for contributors without a database. Set
//! `MCP_USAGE_REQUIRE_BACKENDS=1`, as CI does, to turn a missing backend into a
//! failure instead of a silent skip. Without that, a broken CI service container
//! would look exactly like a passing run.
//!
//! ```sh
//! MCP_USAGE_TEST_POSTGRES_URL=postgres://localhost/usagekit_test \
//! MCP_USAGE_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo test -p mcp-usage-store --all-features --test backends
//! ```

#![cfg(any(feature = "postgres", feature = "redis"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mcp_usage_core::{Call, Method};
use mcp_usage_tower::TaskAttributionStore;

/// Resolve a backend URL, or decide whether absence is a skip or a failure.
fn backend_url(variable: &str) -> Option<String> {
    match std::env::var(variable) {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            assert!(
                std::env::var("MCP_USAGE_REQUIRE_BACKENDS").is_err(),
                "{variable} is not set but MCP_USAGE_REQUIRE_BACKENDS demands a real backend"
            );
            eprintln!("skipping: {variable} is not set");
            None
        }
    }
}

/// Identifiers unique across concurrent tests and repeated runs against a
/// database that is not torn down between them.
fn unique(label: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{label}-{nanos}-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn origin_call() -> Call {
    Call::new(Method::ToolsCall, Some("expensive".to_owned()))
}

/// Every invariant the edge relies on when pricing a durable task.
async fn assert_task_store_contract(store: &dyn TaskAttributionStore) {
    let tenant = unique("tenant");
    let neighbour = unique("tenant");
    let task = unique("task");
    let origin = origin_call();

    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        None,
        "an unknown task must not resolve"
    );

    store.insert(&tenant, &task, origin.clone()).await.unwrap();
    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        Some(origin.clone())
    );

    // A durable task's origin is immutable. A reused or hostile task ID must not
    // replace the price attribution captured the first time, or a caller could
    // retroactively reprice expensive work as cheap work.
    store
        .insert(
            &tenant,
            &task,
            Call::new(Method::ToolsCall, Some("cheap".to_owned())),
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        Some(origin.clone()),
        "the first writer must win"
    );

    // The same task ID belonging to another tenant is a different record.
    store
        .insert(&neighbour, &task, Call::new(Method::PromptsGet, None))
        .await
        .unwrap();
    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        Some(origin.clone()),
        "another tenant's write must not disturb this one"
    );

    // Claiming consumes the record exactly once.
    assert_eq!(
        store.claim(&tenant, &task).await.unwrap(),
        Some(origin.clone())
    );
    assert_eq!(
        store.claim(&tenant, &task).await.unwrap(),
        None,
        "a claimed task must not be claimable again"
    );
    assert_eq!(store.get(&tenant, &task).await.unwrap(), None);

    // The neighbour's record survives its neighbour being claimed.
    assert_eq!(
        store.claim(&neighbour, &task).await.unwrap(),
        Some(Call::new(Method::PromptsGet, None))
    );

    // Extension methods and absent names survive the round trip, since both
    // reach the price book verbatim.
    for call in [
        Call::new(
            Method::Other("io.example/frobnicate".to_owned()),
            Some("job".to_owned()),
        ),
        Call::new(Method::ToolsCall, None),
    ] {
        let id = unique("task");
        store.insert(&tenant, &id, call.clone()).await.unwrap();
        assert_eq!(
            store.claim(&tenant, &id).await.unwrap(),
            Some(call.clone()),
            "round trip failed for {call:?}"
        );
    }

    // Removal is silent about keys that were never there.
    let removable = unique("task");
    store
        .insert(&tenant, &removable, origin.clone())
        .await
        .unwrap();
    store.remove(&tenant, &removable).await.unwrap();
    assert_eq!(store.get(&tenant, &removable).await.unwrap(), None);
    store.remove(&tenant, &unique("task")).await.unwrap();
}

/// Only one caller may claim a completed task, or it bills more than once.
///
/// This is the property that makes horizontal scaling safe, and it is the one
/// the in-process store cannot demonstrate.
async fn assert_claim_is_exclusive_under_contention(store: Arc<dyn TaskAttributionStore>) {
    let tenant = unique("tenant");
    let task = unique("task");
    let origin = origin_call();
    store.insert(&tenant, &task, origin.clone()).await.unwrap();

    let winners = Arc::new(AtomicU64::new(0));
    let mut racers = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let winners = Arc::clone(&winners);
        let tenant = tenant.clone();
        let task = task.clone();
        let expected = origin.clone();
        racers.push(tokio::spawn(async move {
            let claimed = store.claim(&tenant, &task).await.expect("claim");
            if let Some(call) = claimed {
                assert_eq!(call, expected, "a winning claim returned the wrong origin");
                winners.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for racer in racers {
        racer.await.expect("claim task panicked");
    }

    assert_eq!(
        winners.load(Ordering::Relaxed),
        1,
        "exactly one claim may win, otherwise a completed task bills more than once"
    );
}

/// An abandoned task must not be retained forever.
async fn assert_records_expire(store: &dyn TaskAttributionStore) {
    let tenant = unique("tenant");
    let task = unique("task");
    store.insert(&tenant, &task, origin_call()).await.unwrap();
    assert!(store.get(&tenant, &task).await.unwrap().is_some());

    tokio::time::sleep(Duration::from_millis(1_500)).await;

    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        None,
        "the record must not outlive its TTL"
    );
    assert_eq!(
        store.claim(&tenant, &task).await.unwrap(),
        None,
        "an expired record must not be claimable"
    );
}

#[cfg(feature = "redis")]
mod redis_backend {
    use super::{
        Arc, Duration, assert_claim_is_exclusive_under_contention, assert_records_expire,
        assert_task_store_contract, backend_url,
    };
    use mcp_usage_store::RedisTaskStore;

    async fn connect(ttl: Duration) -> Option<RedisTaskStore> {
        let url = backend_url("MCP_USAGE_TEST_REDIS_URL")?;
        Some(
            RedisTaskStore::connect(&url, "mcp-usage-test", ttl)
                .await
                .expect("connect to Redis"),
        )
    }

    #[tokio::test]
    async fn honors_the_task_store_contract() {
        let Some(store) = connect(Duration::from_secs(60)).await else {
            return;
        };
        assert_task_store_contract(&store).await;
    }

    #[tokio::test]
    async fn claims_exactly_once_under_contention() {
        let Some(store) = connect(Duration::from_secs(60)).await else {
            return;
        };
        assert_claim_is_exclusive_under_contention(Arc::new(store)).await;
    }

    #[tokio::test]
    async fn records_expire() {
        let Some(store) = connect(Duration::from_secs(1)).await else {
            return;
        };
        assert_records_expire(&store).await;
    }

    #[tokio::test]
    async fn a_rejected_connection_url_is_reported_without_leaking_it() {
        // A malformed URL fails in `Client::open`, which keeps this instant.
        // Pointing at a closed port would exercise the same `map_err` arm but
        // costs nine seconds of connection-manager backoff on every run,
        // including the runs where every other test in this file skips.
        let error = RedisTaskStore::connect(
            "redis://sentinel-password@host:not-a-port/",
            "mcp-usage-test",
            Duration::from_secs(60),
        )
        .await
        .expect_err("a malformed connection URL must be rejected");

        let rendered = format!("{error}");
        assert!(
            !rendered.contains("sentinel-password") && !rendered.contains("host"),
            "connection failures must not echo the URL: {rendered}"
        );
    }
}

#[cfg(feature = "postgres")]
mod postgres_backend {
    use super::{
        Arc, Duration, assert_claim_is_exclusive_under_contention, assert_records_expire,
        assert_task_store_contract, backend_url, origin_call, unique,
    };
    use mcp_usage_store::PostgresTaskStore;
    use mcp_usage_tower::TaskAttributionStore;

    async fn connect(ttl: Duration) -> Option<PostgresTaskStore> {
        let url = backend_url("MCP_USAGE_TEST_POSTGRES_URL")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect(&url)
            .await
            .expect("connect to PostgreSQL");
        let store = PostgresTaskStore::new(pool, ttl).expect("valid ttl");
        // Idempotent, so every test can assert its own schema is present.
        store.install().await.expect("install schema");
        Some(store)
    }

    #[tokio::test]
    async fn honors_the_task_store_contract() {
        let Some(store) = connect(Duration::from_secs(60)).await else {
            return;
        };
        assert_task_store_contract(&store).await;
    }

    #[tokio::test]
    async fn claims_exactly_once_under_contention() {
        let Some(store) = connect(Duration::from_secs(60)).await else {
            return;
        };
        assert_claim_is_exclusive_under_contention(Arc::new(store)).await;
    }

    #[tokio::test]
    async fn records_expire() {
        let Some(store) = connect(Duration::from_secs(1)).await else {
            return;
        };
        assert_records_expire(&store).await;
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let Some(store) = connect(Duration::from_secs(60)).await else {
            return;
        };
        // Applications with migration-controlled schemas may call this on every
        // boot, so a second run must not fail.
        store.install().await.expect("second install");
        store.install().await.expect("third install");
    }

    #[tokio::test]
    async fn concurrent_installs_all_succeed() {
        // Every instance of a horizontally scaled application calls `install`
        // on boot, and they boot together. `CREATE TABLE IF NOT EXISTS` is not
        // atomic in PostgreSQL, so without serialization the losers of the race
        // fail with a duplicate key violation on `pg_type_typname_nsp_index`
        // and those instances never start.
        //
        // The race only exists while the table is absent, so this must install
        // into an empty schema. Reusing the shared one would mean racing against
        // a table some other test already created, which reproduces nothing.
        let Some(url) = backend_url("MCP_USAGE_TEST_POSTGRES_URL") else {
            return;
        };

        // Identifiers cannot be bound as parameters in PostgreSQL. This one is
        // generated here rather than supplied, and is reduced to `[a-z0-9_]`.
        let schema: String = unique("install_race")
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();

        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to PostgreSQL");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .expect("create scratch schema");

        let options = url
            .parse::<sqlx::postgres::PgConnectOptions>()
            .expect("valid PostgreSQL URL")
            .options([("search_path", schema.as_str())]);
        let scoped = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect_with(options)
            .await
            .expect("connect to the scratch schema");

        let mut booting = Vec::new();
        for instance in 0..8 {
            let pool = scoped.clone();
            booting.push(tokio::spawn(async move {
                PostgresTaskStore::new(pool, Duration::from_secs(60))
                    .expect("valid ttl")
                    .install()
                    .await
                    .unwrap_or_else(|error| {
                        panic!("instance {instance} failed to install: {error}")
                    });
            }));
        }
        let outcome = futures_collect(booting).await;

        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .expect("drop scratch schema");

        outcome.expect("a concurrent install failed");
    }

    /// Join every handle before propagating a panic, so a failure cannot leave
    /// the scratch schema behind.
    async fn futures_collect(
        handles: Vec<tokio::task::JoinHandle<()>>,
    ) -> Result<(), tokio::task::JoinError> {
        let mut first_error = Ok(());
        for handle in handles {
            if let Err(error) = handle.await
                && first_error.is_ok()
            {
                first_error = Err(error);
            }
        }
        first_error
    }

    #[tokio::test]
    async fn pruning_reclaims_only_expired_rows() {
        let Some(short) = connect(Duration::from_secs(1)).await else {
            return;
        };
        let Some(long) = connect(Duration::from_secs(600)).await else {
            return;
        };

        let tenant = unique("tenant");
        let doomed = unique("task");
        let surviving = unique("task");
        short.insert(&tenant, &doomed, origin_call()).await.unwrap();
        long.insert(&tenant, &surviving, origin_call())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let pruned = long.prune_expired().await.expect("prune");

        assert!(pruned >= 1, "the expired row should have been reclaimed");
        assert_eq!(
            long.get(&tenant, &surviving).await.unwrap(),
            Some(origin_call()),
            "pruning must not touch live rows"
        );
    }
}
