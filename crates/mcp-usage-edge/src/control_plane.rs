//! Client adapters that point a sidecar at a hosted control plane.
//!
//! Two things connect the edge to the plane, and both are built around the same
//! rule: **the plane being down must never fail a customer's MCP call.** The
//! tenant store serves from a cached snapshot and the exporter leaves a batch
//! unresolved rather than dropping it, so an outage costs freshness and delays
//! invoicing without costing availability or usage.
//!
//! The one exception is deliberate. A cache older than `max_stale` stops
//! authenticating, because serving revocations that are hours out of date is
//! worse than refusing. That threshold is far longer than the refresh interval,
//! so an ordinary blip never reaches it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use mcp_usage_kit::{
    AggregatedUsage, Limits, MeterEventOutcome, MeterEventProvider, MeterEventProviderError,
    MeterEventProviderFuture, PriceBook, Tenant, TenantStore, Usage, hash_api_key,
};
use serde::{Deserialize, Serialize};

use crate::proxy::HttpsClient;

/// Why a control-plane call failed.
#[derive(Debug, thiserror::Error)]
pub enum PlaneError {
    /// The request never completed.
    #[error("control plane unreachable")]
    Unreachable,
    /// The plane answered with a status the edge cannot use.
    #[error("control plane returned {0}")]
    Status(StatusCode),
    /// The response body did not match the expected shape.
    #[error("control plane response was not understood")]
    Malformed,
}

/// One authenticating key as the plane reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotEntry {
    /// SHA-256 of the tenant key.
    pub api_key_sha256: String,
    /// `Tenant.id`.
    pub tenant_id: String,
    /// `Tenant.billing_customer_id`.
    pub billing_customer_id: String,
    /// `Tenant.prices`.
    pub prices: PriceBook,
    /// Unit quota, absent for unbounded.
    #[serde(default)]
    pub max_units: Option<u64>,
    /// Spend cap in millionths, absent for unbounded.
    #[serde(default)]
    pub max_spend_micros: Option<u64>,
    /// Monetary price of one unit, in millionths.
    #[serde(default)]
    pub unit_price_micros: u64,
    /// Units already committed in the plane's current window.
    #[serde(default)]
    pub committed_units: u64,
    /// Spend already committed in the plane's current window.
    #[serde(default)]
    pub committed_spend_micros: u64,
}

/// A full replacement view of everything this edge may serve.
#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    /// One entry per active key.
    pub tenants: Vec<SnapshotEntry>,
}

#[derive(Debug, Serialize)]
struct UsageOut<'a> {
    identifier: &'a str,
    customer_id: &'a str,
    meter: &'a str,
    units: u64,
    timestamp: u64,
}

#[derive(Debug, Serialize)]
struct UsageBatchOut<'a> {
    events: Vec<UsageOut<'a>>,
}

#[derive(Debug, Deserialize)]
struct OutcomeIn {
    identifier: String,
    outcome: String,
}

#[derive(Debug, Deserialize)]
struct UsageAck {
    outcomes: Vec<OutcomeIn>,
}

/// Shared HTTP access to one control plane.
#[derive(Clone)]
pub struct PlaneClient {
    client: HttpsClient,
    base: String,
    token: String,
    timeout: Duration,
    max_response_bytes: usize,
}

impl std::fmt::Debug for PlaneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a credential and never belongs in a log line.
        f.debug_struct("PlaneClient")
            .field("base", &self.base)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl PlaneClient {
    /// Construct a client for a control-plane base URL.
    #[must_use]
    pub fn new(client: HttpsClient, base_url: &str, token: String, timeout: Duration) -> Self {
        Self {
            client,
            base: base_url.trim_end_matches('/').to_owned(),
            token,
            timeout,
            max_response_bytes: 16 * 1024 * 1024,
        }
    }

    /// Limit bytes retained from any control-plane response.
    #[must_use]
    pub const fn with_max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: http::Method,
        path: &str,
        body: Option<Bytes>,
    ) -> Result<T, PlaneError> {
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(http::header::ACCEPT, "application/json");
        if body.is_some() {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(Full::new(body.unwrap_or_default()))
            .map_err(|_| PlaneError::Malformed)?;

        tokio::time::timeout(self.timeout, async {
            let response = self
                .client
                .request(request)
                .await
                .map_err(|_| PlaneError::Unreachable)?;
            let status = response.status();
            if !status.is_success() {
                return Err(PlaneError::Status(status));
            }
            let bytes = Limited::new(response.into_body(), self.max_response_bytes)
                .collect()
                .await
                .map_err(|_| PlaneError::Malformed)?
                .to_bytes();
            serde_json::from_slice(&bytes).map_err(|_| PlaneError::Malformed)
        })
        .await
        .map_err(|_| PlaneError::Unreachable)?
    }

    /// Fetch the current snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`PlaneError`] when the plane is unreachable, refuses the token,
    /// or answers with a body the edge cannot parse.
    pub async fn snapshot(&self) -> Result<Snapshot, PlaneError> {
        self.call(http::Method::GET, "/v1/edge/snapshot", None)
            .await
    }

    async fn post_usage(&self, batch: &[AggregatedUsage]) -> Result<UsageAck, PlaneError> {
        let payload = UsageBatchOut {
            events: batch
                .iter()
                .map(|usage| UsageOut {
                    identifier: &usage.identifier,
                    customer_id: &usage.customer_id,
                    meter: &usage.meter,
                    units: usage.units,
                    timestamp: usage.timestamp,
                })
                .collect(),
        };
        let body = serde_json::to_vec(&payload).map_err(|_| PlaneError::Malformed)?;
        self.call(
            http::Method::POST,
            "/v1/edge/usage",
            Some(Bytes::from(body)),
        )
        .await
    }
}

/// What the cache holds for one key.
#[derive(Debug, Clone)]
struct Cached {
    tenant: Arc<Tenant>,
    limits: Limits,
    committed: Usage,
    unit_price_micros: u64,
}

#[derive(Debug)]
struct CacheState {
    entries: HashMap<String, Cached>,
    /// `None` until the first successful snapshot.
    refreshed_at: Option<Instant>,
}

/// A [`TenantStore`] backed by a control-plane snapshot.
///
/// `authenticate` is synchronous and sits on the MCP hot path, so it only ever
/// reads an in-memory map. A background task owns every network call.
pub struct ControlPlaneTenantStore {
    state: RwLock<CacheState>,
    max_stale: Duration,
}

impl std::fmt::Debug for ControlPlaneTenantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneTenantStore")
            .field("cached_keys", &self.len())
            .field("max_stale", &self.max_stale)
            .finish_non_exhaustive()
    }
}

impl ControlPlaneTenantStore {
    /// Construct an empty store that refuses everything until its first refresh.
    #[must_use]
    pub fn new(max_stale: Duration) -> Self {
        Self {
            state: RwLock::new(CacheState {
                entries: HashMap::new(),
                refreshed_at: None,
            }),
            max_stale,
        }
    }

    /// Number of keys currently served.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, CacheState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace the cache with a freshly fetched snapshot.
    pub fn apply(&self, snapshot: Snapshot) {
        let entries = snapshot
            .tenants
            .into_iter()
            .map(|entry| {
                let cached = Cached {
                    tenant: Arc::new(
                        Tenant::new(entry.tenant_id, entry.billing_customer_id)
                            .with_prices(entry.prices),
                    ),
                    limits: Limits {
                        max_units: entry.max_units,
                        max_spend_micros: entry.max_spend_micros,
                    },
                    committed: Usage {
                        units: entry.committed_units,
                        spend_micros: entry.committed_spend_micros,
                    },
                    unit_price_micros: entry.unit_price_micros,
                };
                (entry.api_key_sha256, cached)
            })
            .collect();

        let replaced = {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.refreshed_at = Some(Instant::now());
            std::mem::replace(&mut state.entries, entries)
        };
        // Freeing the previous snapshot means dropping a string and a price
        // book per key. Assigning over the field ran that inside the write
        // lock, so every refresh stalled the authentication path for as long as
        // the deallocations took, which scales with the tenant count.
        drop(replaced);
    }

    /// Whether the cache is too old to be trusted.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.read()
            .refreshed_at
            .is_none_or(|at| at.elapsed() > self.max_stale)
    }

    /// The cached tenant for a presented key, without the staleness check.
    ///
    /// For pricing a payment challenge, where the caller has already decided
    /// the request is being refused and only needs the price book. Admission
    /// itself goes through [`Self::quota_for`], which does check staleness.
    #[must_use]
    pub fn tenant_for(&self, api_key: &str) -> Option<Arc<Tenant>> {
        self.read()
            .entries
            .get(&hash_api_key(api_key))
            .map(|cached| cached.tenant.clone())
    }

    /// The limits, committed usage and unit price for a presented key.
    ///
    /// Returns `None` for an unknown key or a stale cache, which the quota gate
    /// treats the same way `authenticate` does.
    #[must_use]
    pub fn quota_for(&self, api_key: &str) -> Option<(Usage, u64, Limits)> {
        let state = self.read();
        if state
            .refreshed_at
            .is_none_or(|at| at.elapsed() > self.max_stale)
        {
            return None;
        }
        state
            .entries
            .get(&hash_api_key(api_key))
            .map(|cached| (cached.committed, cached.unit_price_micros, cached.limits))
    }
}

impl TenantStore for ControlPlaneTenantStore {
    fn authenticate(&self, api_key: &str) -> Option<Arc<Tenant>> {
        let state = self.read();
        // Fail closed rather than serve revocations that are hours out of date.
        // `max_stale` is far longer than the refresh interval, so an ordinary
        // blip never reaches this branch.
        if state
            .refreshed_at
            .is_none_or(|at| at.elapsed() > self.max_stale)
        {
            return None;
        }
        state
            .entries
            .get(&hash_api_key(api_key))
            .map(|cached| cached.tenant.clone())
    }
}

/// Keep a [`ControlPlaneTenantStore`] current. Never returns.
pub async fn refresh_forever(
    store: Arc<ControlPlaneTenantStore>,
    client: PlaneClient,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        match client.snapshot().await {
            Ok(snapshot) => {
                let count = snapshot.tenants.len();
                store.apply(snapshot);
                tracing::debug!(keys = count, "refreshed tenants from the control plane");
            }
            // The previous snapshot keeps serving until `max_stale`. Warning
            // rather than erroring: one failed poll is expected operationally.
            Err(error) => {
                tracing::warn!(%error, "tenant refresh failed; serving the cached snapshot");
            }
        }
        ticker.tick().await;
    }
}

/// A [`MeterEventProvider`] that posts aggregates to the control plane.
#[derive(Debug, Clone)]
pub struct ControlPlaneExporter {
    client: PlaneClient,
}

impl ControlPlaneExporter {
    /// Construct an exporter over a plane client.
    #[must_use]
    pub const fn new(client: PlaneClient) -> Self {
        Self { client }
    }
}

impl MeterEventProvider for ControlPlaneExporter {
    fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a> {
        Box::pin(async move {
            let ack = self.client.post_usage(batch).await.map_err(|error| {
                tracing::warn!(%error, "usage export failed; the batch stays pending");
                match error {
                    PlaneError::Status(status) if status == StatusCode::UNAUTHORIZED => {
                        MeterEventProviderError::new("authentication_failed")
                    }
                    _ => MeterEventProviderError::new("unavailable"),
                }
            })?;

            // The contract makes a wrong outcome count a batch-wide failure, and
            // treats codes as static low-cardinality categories. Misaligned
            // identifiers are the same class of problem: applying them would
            // mark the wrong aggregate settled, so refuse the whole batch.
            if ack.outcomes.len() != batch.len() {
                tracing::error!(
                    expected = batch.len(),
                    received = ack.outcomes.len(),
                    "control plane returned the wrong number of outcomes"
                );
                return Err(MeterEventProviderError::new("malformed_response"));
            }
            for (usage, outcome) in batch.iter().zip(&ack.outcomes) {
                if usage.identifier != outcome.identifier {
                    tracing::error!("control plane returned outcomes out of order");
                    return Err(MeterEventProviderError::new("malformed_response"));
                }
            }

            Ok(ack
                .outcomes
                .iter()
                .map(|outcome| match outcome.outcome.as_str() {
                    "accepted" => MeterEventOutcome::Accepted,
                    "rejected" => MeterEventOutcome::PermanentRejection {
                        code: "invalid_event",
                    },
                    // Anything unrecognized is treated as unresolved rather than
                    // settled, because guessing "accepted" loses money silently
                    // while guessing "retry" only costs another attempt.
                    _ => MeterEventOutcome::RetryableFailure {
                        code: "unavailable",
                    },
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "Zq4vN8xR2tLmK7wP1sB6yH3dF9gJ0cVe";

    fn snapshot_with(units: u64, max_units: Option<u64>) -> Snapshot {
        Snapshot {
            tenants: vec![SnapshotEntry {
                api_key_sha256: hash_api_key(KEY),
                tenant_id: "acme".to_owned(),
                billing_customer_id: "cus_acme".to_owned(),
                prices: PriceBook::flat(1).with_name("sum", 7),
                max_units,
                max_spend_micros: None,
                unit_price_micros: 1000,
                committed_units: units,
                committed_spend_micros: units * 1000,
            }],
        }
    }

    #[test]
    fn an_empty_store_authenticates_nothing() {
        // Before the first snapshot the edge knows nobody, so it must refuse
        // rather than fall open.
        let store = ControlPlaneTenantStore::new(Duration::from_secs(900));
        assert!(store.authenticate(KEY).is_none());
        assert!(store.is_stale());
    }

    #[test]
    fn a_fresh_snapshot_authenticates_and_carries_prices() {
        let store = ControlPlaneTenantStore::new(Duration::from_secs(900));
        store.apply(snapshot_with(0, None));

        let tenant = store.authenticate(KEY).expect("authenticates");
        assert_eq!(tenant.id, "acme");
        assert_eq!(tenant.billing_customer_id, "cus_acme");
        assert_eq!(
            tenant
                .prices
                .units_for(&mcp_usage_kit::Method::ToolsCall, Some("sum")),
            7
        );
        assert!(!store.is_stale());
    }

    #[test]
    fn a_key_absent_from_the_snapshot_is_refused() {
        let store = ControlPlaneTenantStore::new(Duration::from_secs(900));
        store.apply(snapshot_with(0, None));
        assert!(
            store
                .authenticate("bHc7pQ2wR9xT4vN6mK1sB8yH3dF5gJ0c")
                .is_none()
        );
    }

    #[test]
    fn a_refreshed_snapshot_that_drops_a_key_revokes_it() {
        let store = ControlPlaneTenantStore::new(Duration::from_secs(900));
        store.apply(snapshot_with(0, None));
        assert!(store.authenticate(KEY).is_some());

        // Absence from the next snapshot IS the revocation.
        store.apply(Snapshot { tenants: vec![] });
        assert!(store.authenticate(KEY).is_none());
    }

    #[test]
    fn a_cache_older_than_max_stale_fails_closed() {
        let store = ControlPlaneTenantStore::new(Duration::ZERO);
        store.apply(snapshot_with(0, None));
        // With a zero staleness budget the snapshot is already too old, which is
        // the same branch a long outage reaches.
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.is_stale());
        assert!(store.authenticate(KEY).is_none());
        assert!(store.quota_for(KEY).is_none());
    }

    #[test]
    fn quota_state_comes_back_with_the_key() {
        let store = ControlPlaneTenantStore::new(Duration::from_secs(900));
        store.apply(snapshot_with(42, Some(100)));
        let (usage, price, limits) = store.quota_for(KEY).expect("known key");
        assert_eq!(usage.units, 42);
        assert_eq!(usage.spend_micros, 42_000);
        assert_eq!(price, 1000);
        assert_eq!(limits.max_units, Some(100));
    }
}
