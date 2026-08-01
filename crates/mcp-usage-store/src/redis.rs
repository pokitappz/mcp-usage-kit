use std::time::Duration;

use mcp_usage_core::TaskAttribution;
use mcp_usage_tower::{TaskAttributionStore, TaskStoreError, TaskStoreFuture};

use crate::{StoreConfigError, decode_attribution, encode_attribution, encode_hash, valid_prefix};

const CLAIM_SCRIPT: &str = "local value=redis.call('GET',KEYS[1]);if value then redis.call('DEL',KEYS[1]);end;return value";
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Redis or Valkey durable-task attribution with atomic, once-only claims.
#[derive(Clone)]
pub struct RedisTaskStore {
    connection: redis::aio::ConnectionManager,
    key_prefix: String,
    ttl_seconds: i64,
}

impl std::fmt::Debug for RedisTaskStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisTaskStore")
            .field("key_prefix", &self.key_prefix)
            .field("ttl_seconds", &self.ttl_seconds)
            .finish_non_exhaustive()
    }
}

impl RedisTaskStore {
    /// Connect with a reconnecting, multiplexed async connection manager.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for invalid configuration or connection
    /// failure. Connection URLs are never retained for debug output.
    pub async fn connect(
        connection_url: &str,
        key_prefix: impl Into<String>,
        ttl: Duration,
    ) -> Result<Self, StoreConfigError> {
        Self::connect_with_timeout(connection_url, key_prefix, ttl, DEFAULT_OPERATION_TIMEOUT).await
    }

    /// Connect with an explicit bound for connection and command operations.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for invalid configuration or connection
    /// failure. Connection URLs are never retained for debug output.
    pub async fn connect_with_timeout(
        connection_url: &str,
        key_prefix: impl Into<String>,
        ttl: Duration,
        operation_timeout: Duration,
    ) -> Result<Self, StoreConfigError> {
        let key_prefix = key_prefix.into();
        if !valid_prefix(&key_prefix) {
            return Err(StoreConfigError::InvalidPrefix);
        }
        let ttl_seconds = i64::try_from(ttl.as_secs()).map_err(|_| StoreConfigError::InvalidTtl)?;
        if ttl_seconds == 0 {
            return Err(StoreConfigError::InvalidTtl);
        }
        if operation_timeout.is_zero() {
            return Err(StoreConfigError::InvalidTimeout);
        }
        let client =
            redis::Client::open(connection_url).map_err(|_| StoreConfigError::Connection)?;
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Some(operation_timeout))
            .set_response_timeout(Some(operation_timeout));
        let connection = client
            .get_connection_manager_with_config(config)
            .await
            .map_err(|_| StoreConfigError::Connection)?;
        Ok(Self {
            connection,
            key_prefix,
            ttl_seconds,
        })
    }

    fn key(&self, tenant_id: &str, task_id: &str) -> String {
        format!(
            "{}:{}:{}",
            self.key_prefix,
            encode_hash(tenant_id),
            encode_hash(task_id)
        )
    }
}

impl TaskAttributionStore for RedisTaskStore {
    fn insert<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
        attribution: TaskAttribution,
    ) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let _: Option<String> = redis::cmd("SET")
                .arg(self.key(tenant_id, task_id))
                .arg(&encode_attribution(attribution))
                .arg("NX")
                .arg("EX")
                .arg(self.ttl_seconds)
                .query_async(&mut connection)
                .await
                .map_err(|_| TaskStoreError::BackendUnavailable)?;
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<TaskAttribution>> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let payload: Option<Vec<u8>> = redis::cmd("GET")
                .arg(self.key(tenant_id, task_id))
                .query_async(&mut connection)
                .await
                .map_err(|_| TaskStoreError::BackendUnavailable)?;
            payload.as_deref().map(decode_attribution).transpose()
        })
    }

    fn claim<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<TaskAttribution>> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let payload: Option<Vec<u8>> = redis::cmd("EVAL")
                .arg(CLAIM_SCRIPT)
                .arg(1)
                .arg(self.key(tenant_id, task_id))
                .query_async(&mut connection)
                .await
                .map_err(|_| TaskStoreError::BackendUnavailable)?;
            payload.as_deref().map(decode_attribution).transpose()
        })
    }

    fn remove<'a>(&'a self, tenant_id: &'a str, task_id: &'a str) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let _: u64 = redis::cmd("DEL")
                .arg(self.key(tenant_id, task_id))
                .query_async(&mut connection)
                .await
                .map_err(|_| TaskStoreError::BackendUnavailable)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_usage_core::TaskOriginKind;

    #[test]
    fn payload_contains_only_fixed_category_and_resolved_units() {
        let attribution = TaskAttribution::new(TaskOriginKind::Other, 42);
        let payload = encode_attribution(attribution);
        assert_eq!(payload.len(), 10);
        assert_eq!(decode_attribution(&payload).unwrap(), attribution);
    }

    #[tokio::test]
    async fn rejects_zero_operation_timeout_before_connecting() {
        let error = RedisTaskStore::connect_with_timeout(
            "redis://127.0.0.1/",
            "usagekit:tasks",
            Duration::from_secs(60),
            Duration::ZERO,
        )
        .await
        .unwrap_err();

        assert_eq!(error, StoreConfigError::InvalidTimeout);
    }

    #[tokio::test]
    async fn rejects_ttl_larger_than_redis_signed_integer_before_connecting() {
        let oversized = u64::try_from(i64::MAX).unwrap() + 1;
        let error = RedisTaskStore::connect(
            "redis://127.0.0.1:1/",
            "usagekit:tasks",
            Duration::from_secs(oversized),
        )
        .await
        .unwrap_err();

        assert_eq!(error, StoreConfigError::InvalidTtl);
    }
}
