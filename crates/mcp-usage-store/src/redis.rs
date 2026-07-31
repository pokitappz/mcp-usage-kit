use std::time::Duration;

use mcp_usage_core::{Call, Method};
use mcp_usage_tower::{TaskAttributionStore, TaskStoreError, TaskStoreFuture};

use crate::{StoreConfigError, encode_hash, valid_prefix};

const CLAIM_SCRIPT: &str = "local value=redis.call('GET',KEYS[1]);if value then redis.call('DEL',KEYS[1]);end;return value";
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Redis or Valkey durable-task attribution with atomic, once-only claims.
#[derive(Clone)]
pub struct RedisTaskStore {
    connection: redis::aio::ConnectionManager,
    key_prefix: String,
    ttl_seconds: u64,
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
        let ttl_seconds = ttl.as_secs();
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
        call: Call,
    ) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            let payload = serde_json::to_string(&(call.method.as_str(), call.name))
                .map_err(|_| TaskStoreError::InvalidRecord)?;
            let mut connection = self.connection.clone();
            let _: Option<String> = redis::cmd("SET")
                .arg(self.key(tenant_id, task_id))
                .arg(payload)
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
    ) -> TaskStoreFuture<'a, Option<Call>> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let payload: Option<String> = redis::cmd("GET")
                .arg(self.key(tenant_id, task_id))
                .query_async(&mut connection)
                .await
                .map_err(|_| TaskStoreError::BackendUnavailable)?;
            payload.map(|value| decode_call(&value)).transpose()
        })
    }

    fn claim<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<Call>> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let payload: Option<String> = redis::cmd("EVAL")
                .arg(CLAIM_SCRIPT)
                .arg(1)
                .arg(self.key(tenant_id, task_id))
                .query_async(&mut connection)
                .await
                .map_err(|_| TaskStoreError::BackendUnavailable)?;
            payload.map(|value| decode_call(&value)).transpose()
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

fn decode_call(payload: &str) -> Result<Call, TaskStoreError> {
    let (method, name): (String, Option<String>) =
        serde_json::from_str(payload).map_err(|_| TaskStoreError::InvalidRecord)?;
    Ok(Call::new(Method::parse(&method), name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip_preserves_extension_methods() {
        let call = Call::new(
            Method::Other("vendor/run".to_owned()),
            Some("job".to_owned()),
        );
        let payload = serde_json::to_string(&(call.method.as_str(), call.name.clone())).unwrap();
        assert_eq!(decode_call(&payload).unwrap(), call);
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
}
