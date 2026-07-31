//! Distributed durable-task attribution for `UsageKit` for MCP.
//!
//! Backends store SHA-256 digests of tenant and task identifiers, never their
//! plaintext values. Inserts are first-writer-wins and claims are atomic, so a
//! completed task can be accounted for by at most one application instance.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

#[cfg(any(feature = "postgres", feature = "redis", test))]
use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "redis")]
mod redis;

#[cfg(feature = "postgres")]
pub use postgres::PostgresTaskStore;
#[cfg(feature = "redis")]
pub use redis::RedisTaskStore;

/// Configuration or connection failure while constructing a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StoreConfigError {
    /// The time-to-live must be greater than zero and fit the backend format.
    #[error("task-attribution TTL is outside the supported range")]
    InvalidTtl,
    /// Backend operations require a nonzero timeout.
    #[error("task-attribution operation timeout must be greater than zero")]
    InvalidTimeout,
    /// A key prefix was empty, too long, or contained unsupported characters.
    #[error("task-attribution key prefix is invalid")]
    InvalidPrefix,
    /// The backend connection could not be established.
    #[error("task-attribution backend connection failed")]
    Connection,
    /// The backend schema could not be installed or verified.
    #[error("task-attribution backend schema initialization failed")]
    Schema,
}

#[cfg(any(feature = "postgres", feature = "redis", test))]
fn identifier_hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

#[cfg(any(feature = "redis", test))]
fn encode_hash(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = identifier_hash(value);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(any(feature = "redis", test))]
fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 64
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_fixed_length_and_absent_from_keys() {
        let encoded = encode_hash("tenant:private@example.test");
        assert_eq!(encoded.len(), 64);
        assert!(!encoded.contains("private"));
    }

    #[test]
    fn prefixes_are_bounded_and_ascii_only() {
        assert!(valid_prefix("mcp-usage:tasks"));
        assert!(!valid_prefix(""));
        assert!(!valid_prefix("spaces are rejected"));
        assert!(!valid_prefix(&"a".repeat(65)));
    }
}
