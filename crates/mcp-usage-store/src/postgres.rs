use std::time::Duration;
use std::{future::Future, result::Result as StdResult};

use mcp_usage_core::TaskAttribution;
use mcp_usage_tower::{TaskAttributionStore, TaskStoreError, TaskStoreFuture};
use sqlx::PgPool;

use crate::{StoreConfigError, decode_attribution, encode_attribution, identifier_hash};

/// Schema installation, serialized by a transaction-scoped advisory lock.
///
/// `CREATE TABLE IF NOT EXISTS` is not atomic in `PostgreSQL`. Racing sessions
/// each consult the catalog, all conclude the table is absent, and every loser
/// fails with a duplicate key violation on `pg_type_typname_nsp_index`. Taking
/// an advisory lock first serializes the check, so late arrivals find the table
/// already present and do nothing.
///
/// `sqlx::raw_sql` uses the simple query protocol, which `PostgreSQL` executes as
/// a single implicit transaction. The lock is therefore held across the DDL and
/// released at commit, with no explicit transaction object to thread through.
///
/// The lock key is an arbitrary but stable constant, assembled at compile time.
/// It only needs to be distinct from other advisory locks the host application
/// takes on the same database.
const INSTALL_SQL: &str = concat!(
    "SELECT pg_advisory_xact_lock(7594834021775991);\n",
    include_str!("../schema/postgres.sql"),
    "\nSELECT attribution FROM mcp_usage_task_attribution LIMIT 0;"
);
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

/// `PostgreSQL` durable-task attribution with atomic, once-only claims.
#[derive(Clone)]
pub struct PostgresTaskStore {
    pool: PgPool,
    ttl_seconds: i64,
    operation_timeout: Duration,
}

impl std::fmt::Debug for PostgresTaskStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresTaskStore")
            .field("ttl_seconds", &self.ttl_seconds)
            .field("operation_timeout", &self.operation_timeout)
            .finish_non_exhaustive()
    }
}

impl PostgresTaskStore {
    /// Construct a store from an application-owned connection pool.
    ///
    /// # Errors
    ///
    /// Returns [`StoreConfigError::InvalidTtl`] when the TTL is zero or cannot
    /// be represented by `PostgreSQL`'s signed interval input.
    pub fn new(pool: PgPool, ttl: Duration) -> Result<Self, StoreConfigError> {
        Self::with_timeout(pool, ttl, DEFAULT_OPERATION_TIMEOUT)
    }

    /// Construct a store with an explicit bound for every database operation.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid TTL or zero timeout.
    pub fn with_timeout(
        pool: PgPool,
        ttl: Duration,
        operation_timeout: Duration,
    ) -> Result<Self, StoreConfigError> {
        let ttl_seconds = i64::try_from(ttl.as_secs()).map_err(|_| StoreConfigError::InvalidTtl)?;
        if ttl_seconds == 0 {
            return Err(StoreConfigError::InvalidTtl);
        }
        if operation_timeout.is_zero() {
            return Err(StoreConfigError::InvalidTimeout);
        }
        Ok(Self {
            pool,
            ttl_seconds,
            operation_timeout,
        })
    }

    /// Install the idempotent table and expiry index.
    ///
    /// Safe to call concurrently from every instance of a horizontally scaled
    /// application, which is the situation this store exists for. `CREATE TABLE
    /// IF NOT EXISTS` is *not* atomic in `PostgreSQL`: racing sessions check the
    /// catalog, all decide the table is absent, and every loser fails with a
    /// duplicate key violation on `pg_type_typname_nsp_index`. A transaction
    /// scoped advisory lock serializes the check so late arrivals simply find
    /// the table already present and do nothing.
    ///
    /// Applications with migration-controlled schemas can apply
    /// `schema/postgres.sql` themselves and skip this operation.
    ///
    /// # Errors
    ///
    /// Returns a sanitized schema error without exposing connection details.
    pub async fn install(&self) -> Result<(), StoreConfigError> {
        tokio::time::timeout(
            self.operation_timeout,
            sqlx::raw_sql(INSTALL_SQL).execute(&self.pool),
        )
        .await
        .map_err(|_| StoreConfigError::Schema)?
        .map_err(|_| StoreConfigError::Schema)?;
        Ok(())
    }

    /// Delete expired task origins and return the affected row count.
    ///
    /// # Errors
    ///
    /// Returns a sanitized backend error.
    pub async fn prune_expired(&self) -> Result<u64, TaskStoreError> {
        self.run(
            sqlx::query("DELETE FROM mcp_usage_task_attribution WHERE expires_at <= NOW()")
                .execute(&self.pool),
        )
        .await
        .map(|result| result.rows_affected())
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, TaskStoreError>
    where
        F: Future<Output = StdResult<T, sqlx::Error>>,
    {
        tokio::time::timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| TaskStoreError::BackendUnavailable)?
            .map_err(|_| TaskStoreError::BackendUnavailable)
    }
}

impl TaskAttributionStore for PostgresTaskStore {
    fn insert<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
        attribution: TaskAttribution,
    ) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            self.run(
                sqlx::query(
                    "INSERT INTO mcp_usage_task_attribution \
                 (tenant_hash, task_hash, attribution, expires_at) \
                 VALUES ($1, $2, $3, NOW() + ($4 * INTERVAL '1 second')) \
                 ON CONFLICT (tenant_hash, task_hash) DO NOTHING",
                )
                .bind(identifier_hash(tenant_id).to_vec())
                .bind(identifier_hash(task_id).to_vec())
                .bind(encode_attribution(attribution).to_vec())
                .bind(self.ttl_seconds)
                .execute(&self.pool),
            )
            .await?;
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<TaskAttribution>> {
        Box::pin(async move {
            let row: Option<(Vec<u8>,)> = self
                .run(
                    sqlx::query_as(
                        "SELECT attribution FROM mcp_usage_task_attribution \
                 WHERE tenant_hash = $1 AND task_hash = $2 AND expires_at > NOW()",
                    )
                    .bind(identifier_hash(tenant_id).to_vec())
                    .bind(identifier_hash(task_id).to_vec())
                    .fetch_optional(&self.pool),
                )
                .await?;
            row.map(|(attribution,)| decode_attribution(&attribution))
                .transpose()
        })
    }

    fn claim<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<TaskAttribution>> {
        Box::pin(async move {
            let row: Option<(Vec<u8>,)> = self
                .run(
                    sqlx::query_as(
                        "DELETE FROM mcp_usage_task_attribution \
                 WHERE tenant_hash = $1 AND task_hash = $2 AND expires_at > NOW() \
                 RETURNING attribution",
                    )
                    .bind(identifier_hash(tenant_id).to_vec())
                    .bind(identifier_hash(task_id).to_vec())
                    .fetch_optional(&self.pool),
                )
                .await?;
            row.map(|(attribution,)| decode_attribution(&attribution))
                .transpose()
        })
    }

    fn remove<'a>(&'a self, tenant_id: &'a str, task_id: &'a str) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            self.run(
                sqlx::query(
                    "DELETE FROM mcp_usage_task_attribution \
                 WHERE tenant_hash = $1 AND task_hash = $2",
                )
                .bind(identifier_hash(tenant_id).to_vec())
                .bind(identifier_hash(task_id).to_vec())
                .execute(&self.pool),
            )
            .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    #[tokio::test]
    async fn rejects_zero_operation_timeout_without_contacting_postgres() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://usagekit:usagekit@127.0.0.1/usagekit")
            .unwrap();
        let error = PostgresTaskStore::with_timeout(pool, Duration::from_secs(60), Duration::ZERO)
            .unwrap_err();

        assert_eq!(error, StoreConfigError::InvalidTimeout);
    }
}
