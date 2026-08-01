//! Distributed durable-task attribution for `UsageKit` for MCP.
//!
//! Backends store SHA-256 digests of tenant and task identifiers, never their
//! plaintext values. Values contain only a fixed method category and the price
//! resolved when the task was created, never a name, URI, or extension method
//! string. Inserts are first-writer-wins and claims are atomic, so a completed
//! task can be accounted for by at most one application instance.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

#[cfg(any(feature = "postgres", feature = "redis", test))]
use mcp_usage_core::{TaskAttribution, TaskOriginKind};
#[cfg(any(feature = "postgres", feature = "redis", test))]
use mcp_usage_tower::TaskStoreError;
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

#[cfg(any(feature = "postgres", feature = "redis", test))]
fn encode_attribution(attribution: TaskAttribution) -> [u8; 10] {
    let mut encoded = [0_u8; 10];
    encoded[0] = 1;
    encoded[1] = match attribution.origin_kind() {
        TaskOriginKind::ToolsCall => 1,
        TaskOriginKind::ResourcesRead => 2,
        TaskOriginKind::PromptsGet => 3,
        TaskOriginKind::Other => 255,
    };
    encoded[2..].copy_from_slice(&attribution.units().to_be_bytes());
    encoded
}

#[cfg(any(feature = "postgres", feature = "redis", test))]
fn decode_attribution(payload: &[u8]) -> Result<TaskAttribution, TaskStoreError> {
    let [1, kind, units @ ..] = payload else {
        return Err(TaskStoreError::InvalidRecord);
    };
    let origin_kind = match kind {
        1 => TaskOriginKind::ToolsCall,
        2 => TaskOriginKind::ResourcesRead,
        3 => TaskOriginKind::PromptsGet,
        255 => TaskOriginKind::Other,
        _ => return Err(TaskStoreError::InvalidRecord),
    };
    let units = u64::from_be_bytes(
        units
            .try_into()
            .map_err(|_| TaskStoreError::InvalidRecord)?,
    );
    Ok(TaskAttribution::new(origin_kind, units))
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

    #[test]
    fn attribution_encoding_is_fixed_length_and_validated() {
        for kind in [
            TaskOriginKind::ToolsCall,
            TaskOriginKind::ResourcesRead,
            TaskOriginKind::PromptsGet,
            TaskOriginKind::Other,
        ] {
            let attribution = TaskAttribution::new(kind, u64::MAX);
            let encoded = encode_attribution(attribution);
            assert_eq!(encoded.len(), 10);
            assert_eq!(decode_attribution(&encoded).unwrap(), attribution);
        }
        assert_eq!(
            decode_attribution(&[1, 2]),
            Err(TaskStoreError::InvalidRecord)
        );
        assert_eq!(
            decode_attribution(&[2, 1, 0, 0, 0, 0, 0, 0, 0, 1]),
            Err(TaskStoreError::InvalidRecord)
        );
    }
}
