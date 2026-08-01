//! Authorization-aware cache for the six cacheable MCP result methods.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use http::{HeaderMap, StatusCode, Version};
use serde_json::Value;
use sha2::{Digest, Sha256};

use mcp_usage_core::{Call, Method, RequestPeek};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheScope {
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    logical: [u8; 32],
    private_tenant: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedResponse {
    pub status: StatusCode,
    pub version: Version,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Debug, Clone)]
struct Entry {
    response: CachedResponse,
    expires_at: Instant,
    inserted_at: Instant,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<CacheKey, Entry>,
}

#[derive(Debug)]
pub(crate) struct ResponseCache {
    state: Mutex<CacheState>,
    max_entries: usize,
    max_ttl: Duration,
    share_public: bool,
}

impl ResponseCache {
    pub fn new(max_entries: usize, max_ttl: Duration, share_public: bool) -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            max_entries,
            max_ttl,
            share_public,
        }
    }

    pub fn get(
        &self,
        logical: [u8; 32],
        authorization_context: &str,
        request_id: Option<&Value>,
    ) -> Option<CachedResponse> {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.entries.retain(|_, entry| entry.expires_at > now);
        let private = CacheKey {
            logical,
            private_tenant: Some(authorization_context.to_owned()),
        };
        let public = CacheKey {
            logical,
            private_tenant: None,
        };
        let mut response = state
            .entries
            .get(&private)
            .or_else(|| state.entries.get(&public))?
            .response
            .clone();
        if let Some(id) = request_id
            && let Some(object) = response.body.as_object_mut()
        {
            object.insert("id".to_owned(), id.clone());
        }
        Some(response)
    }

    pub fn insert(
        &self,
        logical: [u8; 32],
        authorization_context: &str,
        scope: CacheScope,
        ttl: Duration,
        response: CachedResponse,
    ) {
        let ttl = ttl.min(self.max_ttl);
        if ttl.is_zero() || self.max_entries == 0 {
            return;
        }
        let now = Instant::now();
        let Some(expires_at) = now.checked_add(ttl) else {
            return;
        };
        // An origin declaring `cacheScope: "public"` is asserting the result is
        // tenant-independent. Honoring that places one entry in a bucket every
        // authorization context can read, so a single mislabelled result at the
        // origin becomes a cross-tenant disclosure here. Sharing is therefore
        // opt-in: without it a public result is stored exactly like a private
        // one, and caching less than the spec permits is always legal.
        let effective_public = matches!(scope, CacheScope::Public) && self.share_public;
        let key = CacheKey {
            logical,
            private_tenant: (!effective_public).then(|| authorization_context.to_owned()),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.entries.retain(|_, entry| entry.expires_at > now);
        // Invalidation follows where the entry actually landed rather than what
        // the origin declared. A demoted public result must evict the shared
        // representation it was meant to supersede, not keep it alive.
        if effective_public {
            // A newly-public representation supersedes every private
            // representation of the same logical result.
            state.entries.retain(|existing, _| {
                existing.logical != logical || existing.private_tenant.is_none()
            });
        } else {
            state.entries.remove(&CacheKey {
                logical,
                private_tenant: None,
            });
        }
        if !state.entries.contains_key(&key)
            && state.entries.len() >= self.max_entries
            && let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.inserted_at)
                .map(|(key, _)| key.clone())
        {
            state.entries.remove(&oldest);
        }
        state.entries.insert(
            key,
            Entry {
                response,
                expires_at,
                inserted_at: now,
            },
        );
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RequestMetadata {
    pub request_id: Option<Value>,
    pub cache_key: Option<[u8; 32]>,
    pub is_continuation: bool,
    pub requested_task_id: Option<String>,
}

pub(crate) fn inspect_request(call: &Call, bytes: &[u8]) -> RequestMetadata {
    let parsed: Option<Value> = serde_json::from_slice(bytes).ok();
    let request_id = parsed.as_ref().and_then(|body| body.get("id")).cloned();
    let requested_task_id = if matches!(call.method, Method::TasksGet) {
        parsed
            .as_ref()
            .and_then(|body| body.pointer("/params/taskId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    };
    let RequestPeek { is_continuation } = parsed
        .as_ref()
        .map(mcp_usage_core::peek::request)
        .unwrap_or_default();
    let cache_key = if call.method.is_cacheable()
        && !is_continuation
        && request_id.as_ref().is_some_and(valid_json_rpc_id)
        && parsed
            .as_ref()
            .is_some_and(|body| body_matches_headers(call, body))
    {
        parsed.as_ref().map(|body| logical_key(&call.method, body))
    } else {
        None
    };
    RequestMetadata {
        request_id,
        cache_key,
        is_continuation,
        requested_task_id,
    }
}

fn body_matches_headers(call: &Call, body: &Value) -> bool {
    if body.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }
    if body.get("method").and_then(Value::as_str) != Some(call.method.as_str()) {
        return false;
    }
    let body_name = match call.method {
        Method::ToolsCall | Method::PromptsGet => body.pointer("/params/name"),
        Method::ResourcesRead => body.pointer("/params/uri"),
        _ => return true,
    };
    body_name.and_then(Value::as_str) == call.name.as_deref()
}

fn valid_json_rpc_id(id: &Value) -> bool {
    id.is_null() || id.is_number() || id.is_string()
}

fn logical_key(method: &Method, body: &Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(method.as_str().as_bytes());
    hasher.update([0]);
    if let Some(params) = body.get("params") {
        hasher.update(params.to_string().as_bytes());
    }
    hasher.finalize().into()
}

pub(crate) fn cache_hints(body: &Value) -> Option<(Duration, CacheScope)> {
    let result = body.get("result")?;
    if result.get("resultType").and_then(Value::as_str) != Some("complete") {
        return None;
    }
    let ttl_ms = match result.get("ttlMs").and_then(Value::as_i64) {
        Some(value) if value > 0 => u64::try_from(value).ok()?,
        _ => return None,
    };
    // Missing scope is private at a shared edge. This is stricter
    // than treating an absent wire field as public.
    let scope = match result.get("cacheScope").and_then(Value::as_str) {
        Some("public") => CacheScope::Public,
        Some("private") | None => CacheScope::Private,
        Some(_) => return None,
    };
    Some((Duration::from_millis(ttl_ms), scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stored(body: Value) -> CachedResponse {
        CachedResponse {
            status: StatusCode::OK,
            version: Version::HTTP_11,
            headers: HeaderMap::new(),
            body,
        }
    }

    fn put(cache: &ResponseCache, tenant: &str, scope: CacheScope, body: Value) {
        cache.insert(
            [7; 32],
            tenant,
            scope,
            Duration::from_secs(30),
            stored(body),
        );
    }

    #[test]
    fn public_results_do_not_cross_authorization_contexts_by_default() {
        let cache = ResponseCache::new(16, Duration::from_secs(60), false);
        put(&cache, "tenant-a", CacheScope::Public, json!({"r": "a"}));

        assert!(cache.get([7; 32], "tenant-a", None).is_some());
        assert!(
            cache.get([7; 32], "tenant-b", None).is_none(),
            "an origin declaring `public` must not leak across tenants unless the operator opted in"
        );
    }

    #[test]
    fn public_results_are_shared_once_the_operator_opts_in() {
        let cache = ResponseCache::new(16, Duration::from_secs(60), true);
        put(&cache, "tenant-a", CacheScope::Public, json!({"r": "a"}));

        assert_eq!(cache.get([7; 32], "tenant-b", None).unwrap().body["r"], "a");
    }

    #[test]
    fn private_results_stay_isolated_regardless_of_the_sharing_switch() {
        for share_public in [false, true] {
            let cache = ResponseCache::new(16, Duration::from_secs(60), share_public);
            put(&cache, "tenant-a", CacheScope::Private, json!({"r": "a"}));
            assert!(cache.get([7; 32], "tenant-a", None).is_some());
            assert!(cache.get([7; 32], "tenant-b", None).is_none());
        }
    }

    #[test]
    fn a_later_private_result_evicts_the_shared_representation() {
        let cache = ResponseCache::new(16, Duration::from_secs(60), true);
        put(
            &cache,
            "tenant-a",
            CacheScope::Public,
            json!({"r": "public"}),
        );
        assert!(cache.get([7; 32], "tenant-b", None).is_some());

        put(
            &cache,
            "tenant-a",
            CacheScope::Private,
            json!({"r": "private"}),
        );
        assert!(
            cache.get([7; 32], "tenant-b", None).is_none(),
            "the superseded shared entry must not outlive its private replacement"
        );
        assert_eq!(
            cache.get([7; 32], "tenant-a", None).unwrap().body["r"],
            "private"
        );
    }

    #[test]
    fn a_demoted_public_result_does_not_purge_other_tenants() {
        // With sharing off, one tenant's `public` result is stored privately and
        // must not evict the entry another tenant already holds.
        let cache = ResponseCache::new(16, Duration::from_secs(60), false);
        put(&cache, "tenant-a", CacheScope::Private, json!({"r": "a"}));
        put(&cache, "tenant-b", CacheScope::Public, json!({"r": "b"}));

        assert_eq!(cache.get([7; 32], "tenant-a", None).unwrap().body["r"], "a");
        assert_eq!(cache.get([7; 32], "tenant-b", None).unwrap().body["r"], "b");
    }

    #[test]
    fn cache_key_ignores_json_rpc_id_but_includes_cursor() {
        let call = Call::new(Method::ToolsList, None);
        let one = inspect_request(
            &call,
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"a"}}"#,
        );
        let two = inspect_request(
            &call,
            br#"{"jsonrpc":"2.0","id":99,"method":"tools/list","params":{"cursor":"a"}}"#,
        );
        let other_cursor = inspect_request(
            &call,
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"b"}}"#,
        );
        assert_eq!(one.cache_key, two.cache_key);
        assert_ne!(one.cache_key, other_cursor.cache_key);
    }

    #[test]
    fn continuation_is_never_cacheable() {
        let metadata = inspect_request(
            &Call::new(Method::ResourcesRead, Some("file:///x".to_owned())),
            br#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"file:///x","requestState":"opaque"}}"#,
        );
        assert!(metadata.is_continuation);
        assert!(metadata.cache_key.is_none());
    }

    #[test]
    fn missing_scope_is_private_and_missing_ttl_is_stale() {
        let body = json!({"result":{"resultType":"complete","ttlMs":5000}});
        assert_eq!(
            cache_hints(&body),
            Some((Duration::from_secs(5), CacheScope::Private))
        );
        assert!(cache_hints(&json!({"result":{"resultType":"complete"}})).is_none());
    }

    #[test]
    fn cache_requires_an_id_and_exact_header_body_agreement() {
        let call = Call::new(Method::ResourcesRead, Some("file:///expected".to_owned()));
        assert!(
            inspect_request(
                &call,
                br#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"file:///expected"}}"#,
            )
            .cache_key
            .is_some()
        );
        for body in [
            br#"{"jsonrpc":"2.0","method":"resources/read","params":{"uri":"file:///expected"}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"uri":"file:///expected"}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"file:///other"}}"#.as_slice(),
            br#"{"jsonrpc":"1.0","id":1,"method":"resources/read","params":{"uri":"file:///expected"}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":{},"method":"resources/read","params":{"uri":"file:///expected"}}"#.as_slice(),
        ] {
            assert!(inspect_request(&call, body).cache_key.is_none());
        }
    }
}
