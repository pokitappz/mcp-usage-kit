//! API-key authentication and per-tenant pricing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use mcp_usage_core::PriceBook;
use sha2::{Digest, Sha256};

/// Authenticated tenant configuration used by the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    /// Stable internal tenant identifier.
    pub id: String,
    /// Customer identifier understood by the billing exporter.
    pub billing_customer_id: String,
    /// Per-name and per-method prices for this tenant.
    pub prices: PriceBook,
}

impl Tenant {
    /// Construct a tenant with a flat one-unit price book.
    #[must_use]
    pub fn new(id: impl Into<String>, billing_customer_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            billing_customer_id: billing_customer_id.into(),
            prices: PriceBook::default(),
        }
    }

    /// Replace the tenant's price book.
    #[must_use]
    pub fn with_prices(mut self, prices: PriceBook) -> Self {
        self.prices = prices;
        self
    }
}

/// Resolves a presented high-entropy API key without retaining its plaintext.
pub trait TenantStore: Send + Sync {
    /// Return the matching tenant, or `None` for an invalid key.
    fn authenticate(&self, api_key: &str) -> Option<Tenant>;
}

/// SHA-256 lookup hash for an API key.
///
/// API keys are high-entropy secrets, so a fast cryptographic lookup hash is
/// appropriate; this is not a password hash.
#[must_use]
pub fn hash_api_key(api_key: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(api_key.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Caps how fast credentials can be guessed, across the whole edge.
///
/// Only failures consume budget, so a caller presenting a valid key is never
/// affected no matter how much guessing is happening alongside it. That is what
/// makes a global counter safe here: exhausting it cannot lock out legitimate
/// traffic, it can only turn a wrong key's `401` into a `429`.
///
/// The window is fixed rather than a sliding bucket, which admits a burst across
/// a boundary. That is an acceptable trade for a defense whose purpose is to
/// bound sustained throughput, and it keeps the accounting to two fields.
///
/// This is not a substitute for per-client limiting. The edge cannot see a
/// trustworthy client identity: a source address belongs to the transport, and
/// forwarding headers are attacker controlled. Per-address limits belong in the
/// proxy or load balancer in front of this.
#[derive(Debug)]
pub(crate) struct AuthFailureLimit {
    max_failures: u64,
    window: Duration,
    state: Mutex<FailureWindow>,
}

#[derive(Debug)]
struct FailureWindow {
    started: Instant,
    failures: u64,
}

impl AuthFailureLimit {
    pub(crate) fn new(max_failures: u64, window: Duration) -> Self {
        Self {
            max_failures,
            window,
            state: Mutex::new(FailureWindow {
                started: Instant::now(),
                failures: 0,
            }),
        }
    }

    /// Whether guessing should be refused before a credential is even read.
    pub(crate) fn is_exhausted(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut state, self.window);
        state.failures >= self.max_failures
    }

    /// Charge one failed authentication against the current window.
    pub(crate) fn record_failure(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut state, self.window);
        state.failures = state.failures.saturating_add(1);
    }

    fn expire(state: &mut FailureWindow, window: Duration) {
        if state.started.elapsed() >= window {
            state.started = Instant::now();
            state.failures = 0;
        }
    }
}

/// Why an API key was refused as too weak to be safe.
///
/// See [`validate_api_key_strength`] for what is and is not being claimed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeakApiKey {
    /// Shorter than [`MIN_API_KEY_BYTES`].
    TooShort,
    /// Built from too small an alphabet to carry meaningful entropy.
    TooFewDistinctSymbols,
    /// Symbol distribution is too skewed, as in a long run of one character.
    TooPredictable,
}

impl std::fmt::Display for WeakApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooShort => "API key is shorter than 24 bytes",
            Self::TooFewDistinctSymbols => "API key uses fewer than 10 distinct characters",
            Self::TooPredictable => "API key symbol distribution carries too little entropy",
        })
    }
}

impl std::error::Error for WeakApiKey {}

/// Shortest API key accepted by [`validate_api_key_strength`].
pub const MIN_API_KEY_BYTES: usize = 24;
const MIN_DISTINCT_SYMBOLS: usize = 10;
const MIN_BITS_PER_SYMBOL: f64 = 3.0;

/// Reject API keys that are obviously too weak to be compared by digest.
///
/// Keys are looked up by SHA-256, which is a fast lookup hash and deliberately
/// not a password hash. That is the right choice for a high-entropy secret and
/// the wrong one for a guessable string: a low-entropy key is brute forceable
/// online, and offline in moments if the digest table ever leaks.
///
/// This is a guardrail against obvious mistakes, not a certificate of strength.
/// It measures length, alphabet size, and the Shannon entropy of the observed
/// symbol distribution. It cannot see structure: a long repeating pattern drawn
/// from a wide alphabet will pass. Generate keys from a CSPRNG rather than
/// relying on this to grade a hand-written one.
///
/// # Errors
///
/// Returns the specific [`WeakApiKey`] reason the key was refused.
pub fn validate_api_key_strength(api_key: &str) -> Result<(), WeakApiKey> {
    let bytes = api_key.as_bytes();
    if bytes.len() < MIN_API_KEY_BYTES {
        return Err(WeakApiKey::TooShort);
    }

    let mut counts = [0usize; 256];
    for &byte in bytes {
        counts[usize::from(byte)] += 1;
    }
    let distinct = counts.iter().filter(|count| **count > 0).count();
    if distinct < MIN_DISTINCT_SYMBOLS {
        return Err(WeakApiKey::TooFewDistinctSymbols);
    }

    // Shannon entropy of the observed distribution, in bits per symbol.
    #[expect(
        clippy::cast_precision_loss,
        reason = "key lengths are far below the f64 integer precision limit"
    )]
    let total = bytes.len() as f64;
    let bits_per_symbol: f64 = counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "counts are bounded by the key length"
            )]
            let probability = *count as f64 / total;
            -probability * probability.log2()
        })
        .sum();
    if bits_per_symbol < MIN_BITS_PER_SYMBOL {
        return Err(WeakApiKey::TooPredictable);
    }
    Ok(())
}

/// Mutable in-memory tenant table for development and embedded deployments.
#[derive(Clone, Default)]
pub struct InMemoryTenantStore {
    by_key_hash: Arc<RwLock<HashMap<String, Tenant>>>,
}

impl std::fmt::Debug for InMemoryTenantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryTenantStore")
            .field("configured_keys", &self.len())
            .finish_non_exhaustive()
    }
}

impl InMemoryTenantStore {
    /// Construct an empty tenant table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a tenant under the presented plaintext API key.
    ///
    /// The key is checked by [`validate_api_key_strength`] first, because a
    /// digest lookup only protects a secret that was hard to guess to begin
    /// with. Use [`InMemoryTenantStore::insert_unchecked`] when the key has
    /// already been validated elsewhere.
    ///
    /// # Errors
    ///
    /// Returns [`WeakApiKey`] without inserting anything.
    pub fn insert(&self, api_key: &str, tenant: Tenant) -> Result<(), WeakApiKey> {
        validate_api_key_strength(api_key)?;
        self.insert_unchecked(api_key, tenant);
        Ok(())
    }

    /// Insert or replace a tenant without checking the key's strength.
    ///
    /// For tests, for fixtures, and for deployments that validate keys at the
    /// boundary where they are issued. Everything else should prefer
    /// [`InMemoryTenantStore::insert`].
    pub fn insert_unchecked(&self, api_key: &str, tenant: Tenant) {
        let mut tenants = self
            .by_key_hash
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tenants.insert(hash_api_key(api_key), tenant);
    }

    /// Number of configured keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key_hash
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether no keys are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TenantStore for InMemoryTenantStore {
    fn authenticate(&self, api_key: &str) -> Option<Tenant> {
        self.by_key_hash
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&hash_api_key(api_key))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realistic_generated_keys_are_accepted() {
        for key in [
            // 32 random bytes, base64. The shape `openssl rand -base64 32` gives.
            "kZ2mQx7vR4tYbN8pL1sW3eH6jA5cF0uD9gK2nM4oP7Q=",
            // 128-bit hex, a smaller alphabet but still ample entropy.
            "9f3a1c7e5b2d8046af91c3e57d0b2a64",
            // A prefixed provider-style key.
            "mcp_sk_live_7Hq2vXm9RtZp4LbN6wCk3JdY",
        ] {
            assert_eq!(validate_api_key_strength(key), Ok(()), "rejected {key}");
        }
    }

    #[test]
    fn obviously_weak_keys_are_refused() {
        for (key, expected) in [
            ("secret", WeakApiKey::TooShort),
            ("development-key", WeakApiKey::TooShort),
            ("password123", WeakApiKey::TooShort),
            // Long enough, but drawn from almost no alphabet.
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                WeakApiKey::TooFewDistinctSymbols,
            ),
            (
                "abababababababababababababababab",
                WeakApiKey::TooFewDistinctSymbols,
            ),
            // Wide alphabet, but one symbol dominates the distribution.
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabcdefghij",
                WeakApiKey::TooPredictable,
            ),
        ] {
            assert_eq!(
                validate_api_key_strength(key),
                Err(expected),
                "accepted {key}"
            );
        }
    }

    #[test]
    fn the_validating_insert_refuses_to_store_a_weak_key() {
        let store = InMemoryTenantStore::new();
        assert_eq!(
            store.insert("development-key", Tenant::new("acme", "cus_acme")),
            Err(WeakApiKey::TooShort)
        );
        assert!(store.is_empty(), "a refused key must not be stored");
        assert!(store.authenticate("development-key").is_none());

        // The escape hatch exists for fixtures and for keys validated elsewhere.
        store.insert_unchecked("development-key", Tenant::new("acme", "cus_acme"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn the_failure_budget_only_charges_failures() {
        let limit = AuthFailureLimit::new(3, Duration::from_secs(60));
        assert!(!limit.is_exhausted());

        // Successes never touch it, so valid traffic cannot exhaust the budget.
        for _ in 0..100 {
            assert!(!limit.is_exhausted());
        }

        limit.record_failure();
        limit.record_failure();
        assert!(!limit.is_exhausted(), "still within budget");
        limit.record_failure();
        assert!(limit.is_exhausted(), "the budget is spent");
    }

    #[test]
    fn the_failure_budget_recovers_after_its_window() {
        let limit = AuthFailureLimit::new(1, Duration::from_millis(50));
        limit.record_failure();
        assert!(limit.is_exhausted());

        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !limit.is_exhausted(),
            "a new window must restore the budget"
        );
    }

    #[test]
    fn stores_only_hashes_and_resolves_keys() {
        let store = InMemoryTenantStore::new();
        store.insert_unchecked("mcp_secret", Tenant::new("acme", "cus_acme"));
        assert_eq!(store.len(), 1);
        assert_eq!(store.authenticate("mcp_secret").unwrap().id, "acme");
        assert!(store.authenticate("wrong").is_none());
        assert_ne!(hash_api_key("mcp_secret"), "mcp_secret");
        assert!(!format!("{store:?}").contains(&hash_api_key("mcp_secret")));
    }
}
