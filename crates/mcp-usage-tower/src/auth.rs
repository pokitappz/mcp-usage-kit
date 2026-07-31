//! API-key authentication and per-tenant pricing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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
    pub fn insert(&self, api_key: &str, tenant: Tenant) {
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
    fn stores_only_hashes_and_resolves_keys() {
        let store = InMemoryTenantStore::new();
        store.insert("mcp_secret", Tenant::new("acme", "cus_acme"));
        assert_eq!(store.len(), 1);
        assert_eq!(store.authenticate("mcp_secret").unwrap().id, "acme");
        assert!(store.authenticate("wrong").is_none());
        assert_ne!(hash_api_key("mcp_secret"), "mcp_secret");
        assert!(!format!("{store:?}").contains(&hash_api_key("mcp_secret")));
    }
}
