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

use mcp_usage_core::{TaskAttribution, TaskOriginKind};
use mcp_usage_tower::TaskAttributionStore;
use sha2::{Digest, Sha256};

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

fn identifier_hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn identifier_hash_hex(value: &str) -> String {
    use std::fmt::Write;

    identifier_hash(value)
        .iter()
        .fold(String::with_capacity(64), |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
            encoded
        })
}

const fn origin_attribution() -> TaskAttribution {
    TaskAttribution::new(TaskOriginKind::ToolsCall, 250)
}

/// Every invariant the edge relies on when pricing a durable task.
async fn assert_task_store_contract(store: &dyn TaskAttributionStore) {
    let tenant = unique("tenant");
    let neighbour = unique("tenant");
    let task = unique("task");
    let origin = origin_attribution();

    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        None,
        "an unknown task must not resolve"
    );

    store.insert(&tenant, &task, origin).await.unwrap();
    assert_eq!(store.get(&tenant, &task).await.unwrap(), Some(origin));

    // A durable task's origin is immutable. A reused or hostile task ID must not
    // replace the price attribution captured the first time, or a caller could
    // retroactively reprice expensive work as cheap work.
    store
        .insert(
            &tenant,
            &task,
            TaskAttribution::new(TaskOriginKind::ToolsCall, 1),
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        Some(origin),
        "the first writer must win"
    );

    // The same task ID belonging to another tenant is a different record.
    store
        .insert(
            &neighbour,
            &task,
            TaskAttribution::new(TaskOriginKind::PromptsGet, 5),
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(&tenant, &task).await.unwrap(),
        Some(origin),
        "another tenant's write must not disturb this one"
    );

    // Claiming consumes the record exactly once.
    assert_eq!(store.claim(&tenant, &task).await.unwrap(), Some(origin));
    assert_eq!(
        store.claim(&tenant, &task).await.unwrap(),
        None,
        "a claimed task must not be claimable again"
    );
    assert_eq!(store.get(&tenant, &task).await.unwrap(), None);

    // The neighbour's record survives its neighbour being claimed.
    assert_eq!(
        store.claim(&neighbour, &task).await.unwrap(),
        Some(TaskAttribution::new(TaskOriginKind::PromptsGet, 5))
    );

    // Every fixed category and the full unsigned price range survive storage.
    for attribution in [
        TaskAttribution::new(TaskOriginKind::Other, u64::MAX),
        TaskAttribution::new(TaskOriginKind::ResourcesRead, 0),
    ] {
        let id = unique("task");
        store.insert(&tenant, &id, attribution).await.unwrap();
        assert_eq!(
            store.claim(&tenant, &id).await.unwrap(),
            Some(attribution),
            "round trip failed for {attribution:?}"
        );
    }

    // Removal is silent about keys that were never there.
    let removable = unique("task");
    store.insert(&tenant, &removable, origin).await.unwrap();
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
    let origin = origin_attribution();
    store.insert(&tenant, &task, origin).await.unwrap();

    let winners = Arc::new(AtomicU64::new(0));
    let mut racers = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let winners = Arc::clone(&winners);
        let tenant = tenant.clone();
        let task = task.clone();
        let expected = origin;
        racers.push(tokio::spawn(async move {
            let claimed = store.claim(&tenant, &task).await.expect("claim");
            if let Some(attribution) = claimed {
                assert_eq!(
                    attribution, expected,
                    "a winning claim returned the wrong origin"
                );
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
    store
        .insert(&tenant, &task, origin_attribution())
        .await
        .unwrap();
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
        assert_task_store_contract, backend_url, identifier_hash_hex, unique,
    };
    use mcp_usage_core::{Call, Method, PriceBook, TaskAttribution};
    use mcp_usage_store::RedisTaskStore;
    use mcp_usage_tower::TaskAttributionStore;

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
    async fn raw_record_contains_no_resource_uri_or_plaintext_identifier() {
        let Some(url) = backend_url("MCP_USAGE_TEST_REDIS_URL") else {
            return;
        };
        let store = RedisTaskStore::connect(&url, "mcp-usage-test", Duration::from_secs(60))
            .await
            .expect("connect to Redis");
        let tenant = unique("private-tenant@example.test");
        let task = unique("private-task");
        let private_uri = "file:///customers/private@example.test/record";
        let call = Call::new(Method::ResourcesRead, Some(private_uri.to_owned()));
        let attribution =
            TaskAttribution::from_call(&call, &PriceBook::flat(1).with_name(private_uri, 77));
        store.insert(&tenant, &task, attribution).await.unwrap();

        let key = format!(
            "mcp-usage-test:{}:{}",
            identifier_hash_hex(&tenant),
            identifier_hash_hex(&task)
        );
        let client = redis::Client::open(url).expect("valid Redis URL");
        let mut connection = client
            .get_multiplexed_async_connection()
            .await
            .expect("connect for raw inspection");
        let payload: Vec<u8> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .expect("read raw attribution");

        assert_eq!(payload.len(), 10);
        assert!(!key.contains(&tenant));
        assert!(!key.contains(&task));
        assert!(
            !payload
                .windows(private_uri.len())
                .any(|part| part == private_uri.as_bytes())
        );
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
        assert_task_store_contract, backend_url, identifier_hash, origin_attribution, unique,
    };
    use mcp_usage_core::{Call, Method, PriceBook, TaskAttribution};
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
    async fn raw_record_contains_no_resource_uri_or_name_columns() {
        let Some(url) = backend_url("MCP_USAGE_TEST_POSTGRES_URL") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to PostgreSQL");
        let store =
            PostgresTaskStore::new(pool.clone(), Duration::from_secs(60)).expect("valid ttl");
        store.install().await.expect("install schema");
        let tenant = unique("private-tenant@example.test");
        let task = unique("private-task");
        let private_uri = "file:///customers/private@example.test/record";
        let call = Call::new(Method::ResourcesRead, Some(private_uri.to_owned()));
        let attribution =
            TaskAttribution::from_call(&call, &PriceBook::flat(1).with_name(private_uri, 77));
        store.insert(&tenant, &task, attribution).await.unwrap();

        let (payload,): (Vec<u8>,) = sqlx::query_as(
            "SELECT attribution FROM mcp_usage_task_attribution \
             WHERE tenant_hash = $1 AND task_hash = $2",
        )
        .bind(identifier_hash(&tenant).to_vec())
        .bind(identifier_hash(&task).to_vec())
        .fetch_one(&pool)
        .await
        .expect("read raw attribution");
        let columns: Vec<(String,)> = sqlx::query_as(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = current_schema() \
             AND table_name = 'mcp_usage_task_attribution'",
        )
        .fetch_all(&pool)
        .await
        .expect("inspect attribution schema");

        assert_eq!(payload.len(), 10);
        assert!(
            !payload
                .windows(private_uri.len())
                .any(|part| part == private_uri.as_bytes())
        );
        assert!(
            !columns
                .iter()
                .any(|(column,)| column == "method" || column == "name")
        );
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
        short
            .insert(&tenant, &doomed, origin_attribution())
            .await
            .unwrap();
        long.insert(&tenant, &surviving, origin_attribution())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let pruned = long.prune_expired().await.expect("prune");

        assert!(pruned >= 1, "the expired row should have been reclaimed");
        assert_eq!(
            long.get(&tenant, &surviving).await.unwrap(),
            Some(origin_attribution()),
            "pruning must not touch live rows"
        );
    }
}
