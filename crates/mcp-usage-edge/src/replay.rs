//! Shared, atomic payment proof reservations.
//!
//! Redis must retain these keys across sidecar restarts, with persistence enabled
//! and a no-eviction policy. An unavailable store always refuses paid admission.

use std::{future::Future, pin::Pin, time::Duration};

/// A replay-store failure without backend or credential details.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("payment replay store unavailable or misconfigured")]
pub struct ReplayStoreError;

/// An owned reservation. Only its owner may release it after explicit rejection.
pub struct Reservation {
    pub(crate) key: String,
    pub(crate) owner: String,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation").finish_non_exhaustive()
    }
}

/// A bounded asynchronous replay-store operation.
pub type ReplayFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ReplayStoreError>> + Send + 'a>>;

/// Shared reservation backend. Successful reservations survive cancellation.
pub trait ReplayStore: Send + Sync {
    /// Atomically reserve a challenge, returning `None` if already held.
    fn reserve<'a>(
        &'a self,
        key: &'a str,
        retain_for: Duration,
    ) -> ReplayFuture<'a, Option<Reservation>>;
    /// Release only this owner's reservation after a definite non-settlement.
    fn release<'a>(&'a self, reservation: &'a Reservation) -> ReplayFuture<'a, ()>;
}

/// Redis-backed replay protection shared across sidecar instances.
#[cfg(feature = "redis")]
#[derive(Clone)]
pub struct RedisReplayStore {
    connection: redis::aio::ConnectionManager,
    prefix: String,
    timeout: Duration,
}

#[cfg(feature = "redis")]
impl std::fmt::Debug for RedisReplayStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisReplayStore")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "redis")]
impl RedisReplayStore {
    /// Connect to a shared store. Remote connections require verified TLS.
    ///
    /// # Errors
    /// Rejects insecure endpoints, invalid prefixes/timeouts and connection failures.
    pub async fn connect(
        url: &str,
        prefix: &str,
        timeout: Duration,
    ) -> Result<Self, ReplayStoreError> {
        use redis::IntoConnectionInfo;
        let info = url.into_connection_info().map_err(|_| ReplayStoreError)?;
        let secure = match info.addr() {
            redis::ConnectionAddr::Tcp(host, _) => host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()),
            redis::ConnectionAddr::TcpTls { insecure, .. } => !insecure,
            _ => false,
        };
        if !secure
            || timeout.is_zero()
            || prefix.is_empty()
            || prefix.len() > 128
            || !prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':'))
        {
            return Err(ReplayStoreError);
        }
        let client = redis::Client::open(info).map_err(|_| ReplayStoreError)?;
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Some(timeout))
            .set_response_timeout(Some(timeout));
        let connection =
            tokio::time::timeout(timeout, client.get_connection_manager_with_config(config))
                .await
                .map_err(|_| ReplayStoreError)?
                .map_err(|_| ReplayStoreError)?;
        Ok(Self {
            connection,
            prefix: prefix.to_owned(),
            timeout,
        })
    }
}

#[cfg(feature = "redis")]
impl ReplayStore for RedisReplayStore {
    fn reserve<'a>(
        &'a self,
        key: &'a str,
        retain_for: Duration,
    ) -> ReplayFuture<'a, Option<Reservation>> {
        Box::pin(async move {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            use sha2::{Digest, Sha256};
            let millis = i64::try_from(retain_for.as_millis()).map_err(|_| ReplayStoreError)?;
            if millis <= 0 {
                return Err(ReplayStoreError);
            }
            let key = format!(
                "{}:{}",
                self.prefix,
                URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))
            );
            let owner = uuid::Uuid::new_v4().to_string();
            let mut connection = self.connection.clone();
            let result: Option<String> = tokio::time::timeout(
                self.timeout,
                redis::cmd("SET")
                    .arg(&key)
                    .arg(&owner)
                    .arg("NX")
                    .arg("PX")
                    .arg(millis)
                    .query_async(&mut connection),
            )
            .await
            .map_err(|_| ReplayStoreError)?
            .map_err(|_| ReplayStoreError)?;
            Ok(result.map(|_| Reservation { key, owner }))
        })
    }

    fn release<'a>(&'a self, reservation: &'a Reservation) -> ReplayFuture<'a, ()> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let _: i64 = tokio::time::timeout(self.timeout,
                redis::cmd("EVAL").arg("if redis.call('GET',KEYS[1]) == ARGV[1] then return redis.call('DEL',KEYS[1]) else return 0 end")
                    .arg(1).arg(&reservation.key).arg(&reservation.owner).query_async(&mut connection)
            ).await.map_err(|_| ReplayStoreError)?.map_err(|_| ReplayStoreError)?;
            Ok(())
        })
    }
}

/// Process-local backend for tests and explicitly local embedding only.
/// The sidecar never selects this backend for payments.
#[derive(Default)]
pub struct InMemoryReplayStore {
    entries: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
}

impl ReplayStore for InMemoryReplayStore {
    fn reserve<'a>(
        &'a self,
        key: &'a str,
        retain_for: Duration,
    ) -> ReplayFuture<'a, Option<Reservation>> {
        Box::pin(async move {
            let now = std::time::Instant::now();
            let expiry = now
                .checked_add(retain_for)
                .filter(|_| !retain_for.is_zero())
                .ok_or(ReplayStoreError)?;
            let mut entries = self.entries.lock().map_err(|_| ReplayStoreError)?;
            entries.retain(|_, (expires, _)| *expires > now);
            if entries.contains_key(key) {
                return Ok(None);
            }
            if entries.len() >= 100_000 {
                return Err(ReplayStoreError);
            }
            let owner = uuid::Uuid::new_v4().to_string();
            entries.insert(key.to_owned(), (expiry, owner.clone()));
            Ok(Some(Reservation {
                key: key.to_owned(),
                owner,
            }))
        })
    }
    fn release<'a>(&'a self, reservation: &'a Reservation) -> ReplayFuture<'a, ()> {
        Box::pin(async move {
            let mut entries = self.entries.lock().map_err(|_| ReplayStoreError)?;
            if entries
                .get(&reservation.key)
                .is_some_and(|(_, owner)| owner == &reservation.owner)
            {
                entries.remove(&reservation.key);
            }
            Ok(())
        })
    }
}
