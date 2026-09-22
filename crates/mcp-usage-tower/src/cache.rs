//! Authorization-aware cache for the six cacheable MCP result methods.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::{HeaderMap, StatusCode, Version};
use serde::Deserialize;
use serde::de::IgnoredAny;
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
    /// Shared rather than owned so a lookup clones a pointer under the lock
    /// instead of a whole response. A cached `tools/list` is the largest body
    /// the edge holds, and every hit used to deep-copy it before any other
    /// reader could take the mutex.
    response: Arc<CachedResponse>,
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

    /// Look up a cached response, preferring the caller's private entry.
    ///
    /// Expiry is checked per key rather than by sweeping the map. A lookup runs
    /// on every cacheable request, and sweeping made it cost time proportional
    /// to the whole cache while holding the lock, so raising `max_entries`
    /// slowed down every reader. Whichever of the two keys is examined and
    /// found expired is dropped here, and [`ResponseCache::insert`] sweeps
    /// globally when the cache fills, so expired responses do not accumulate.
    ///
    /// The caller's JSON-RPC id is applied by [`render_with_id`] afterwards,
    /// outside the lock, so nothing but two hash lookups happens inside it.
    pub fn get(
        &self,
        logical: [u8; 32],
        authorization_context: &str,
    ) -> Option<Arc<CachedResponse>> {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let private = CacheKey {
            logical,
            private_tenant: Some(authorization_context.to_owned()),
        };
        let public = CacheKey {
            logical,
            private_tenant: None,
        };
        // The private entry wins, but only while it is live: an expired private
        // representation must not shadow a still-valid shared one, which is what
        // the sweep used to guarantee.
        let mut response = None;
        for key in [private, public] {
            match state.entries.get(&key) {
                Some(entry) if entry.expires_at > now => {
                    response = Some(Arc::clone(&entry.response));
                    break;
                }
                Some(_) => {
                    state.entries.remove(&key);
                }
                None => {}
            }
        }
        response
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
        // Sweeping on every insert cost time proportional to the whole cache,
        // under the lock, to reclaim nothing on a cache that is not full.
        // Expiry only has to hold before eviction decides what to drop, and a
        // lookup already reaps the keys it reads.
        if state.entries.len() >= self.max_entries {
            state.entries.retain(|_, entry| entry.expires_at > now);
        }
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
                response: Arc::new(response),
                expires_at,
                inserted_at: now,
            },
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequestMetadata {
    pub request_id: Option<Value>,
    pub cache_key: Option<[u8; 32]>,
    pub is_continuation: bool,
    pub requested_task_id: Option<String>,
}

/// The four facts a non-cacheable request has to give up, and nothing else.
///
/// Deserializing into this instead of a [`Value`] is the difference between
/// allocating a node per key in the body and allocating almost nothing.
/// `tools/call` is the method with the largest bodies and the highest request
/// rate, and it is never cacheable, so on the busiest path the whole tree was
/// built and dropped unread.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanRequest {
    /// `default` plus an explicit reader keeps the distinction a plain
    /// `Option<Value>` would erase: an absent `id` is `None`, an `id` of
    /// `null` is `Some(Value::Null)`. Only the first is a notification.
    #[serde(deserialize_with = "present_value")]
    id: Option<Value>,
    params: LeanParams,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanParams {
    #[serde(rename = "taskId")]
    task_id: Option<String>,
    /// `Option<IgnoredAny>` is exactly the `is_some_and(|v| !v.is_null())` test
    /// the `Value` path runs: absent and `null` both read as `None`, any other
    /// value reads as `Some` without being materialized.
    #[serde(rename = "inputResponses")]
    input_responses: Option<IgnoredAny>,
    #[serde(rename = "requestState")]
    request_state: Option<IgnoredAny>,
}

impl LeanParams {
    const fn is_continuation(&self) -> bool {
        self.input_responses.is_some() || self.request_state.is_some()
    }
}

fn present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

pub(crate) fn inspect_request(call: &Call, bytes: &[u8]) -> RequestMetadata {
    // Only a cacheable method needs the whole body: the cache key hashes the
    // rendered `params` and the agreement check reads the name out of them.
    // Everything else needs four fields.
    if !call.method.is_cacheable()
        && let Ok(lean) = serde_json::from_slice::<LeanRequest>(bytes)
    {
        return RequestMetadata {
            request_id: lean.id,
            cache_key: None,
            is_continuation: lean.params.is_continuation(),
            requested_task_id: if matches!(call.method, Method::TasksGet) {
                lean.params.task_id
            } else {
                None
            },
        };
    }
    // A body whose `params` is an array or `null`, or which is not an object at
    // all, fails the narrow parse. Falling through costs a second pass on an
    // unusual shape rather than dropping the request id.
    inspect_whole_body(call, bytes)
}

pub(crate) fn inspect_whole_body(call: &Call, bytes: &[u8]) -> RequestMetadata {
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

/// Serialize a cached body with the caller's JSON-RPC id in place of the
/// stored one.
///
/// The stored [`Value`] is shared between every hit, so it cannot be mutated.
/// Cloning it to overwrite one field meant deep-copying the whole tree, which
/// for the cache's largest entries - a `tools/list` result carrying a schema
/// per tool - is most of the work a cache hit was supposed to avoid.
/// Substituting during serialization walks the tree once, which the caller has
/// to do anyway to produce bytes.
pub(crate) fn render_with_id(body: &Value, request_id: Option<&Value>) -> String {
    match (request_id, body.as_object()) {
        (Some(id), Some(object)) => {
            serde_json::to_string(&BodyWithId { object, id }).unwrap_or_else(|_| body.to_string())
        }
        _ => body.to_string(),
    }
}

struct BodyWithId<'a> {
    object: &'a serde_json::Map<String, Value>,
    id: &'a Value,
}

impl serde::Serialize for BodyWithId<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        // A response that carries no `id` still gets one, which is what
        // inserting into the map did.
        let appended = usize::from(!self.object.contains_key("id"));
        let mut map = serializer.serialize_map(Some(self.object.len() + appended))?;
        for (key, value) in self.object {
            if key == "id" {
                map.serialize_entry(key, self.id)?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        if appended == 1 {
            map.serialize_entry("id", self.id)?;
        }
        map.end()
    }
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
    fn an_expired_entry_is_never_served_and_does_not_linger() {
        // Lookups no longer sweep the whole map, so expiry has to hold on the
        // key actually being read, and the dead entry has to go with it rather
        // than keeping a stale tenant's body in memory past its TTL.
        let cache = ResponseCache::new(16, Duration::from_secs(60), false);
        cache.insert(
            [7; 32],
            "tenant-a",
            CacheScope::Private,
            Duration::from_millis(20),
            stored(json!({"r": "a"})),
        );
        assert_eq!(cache.len(), 1);
        assert!(cache.get([7; 32], "tenant-a").is_some());

        std::thread::sleep(Duration::from_millis(50));

        assert!(
            cache.get([7; 32], "tenant-a").is_none(),
            "an expired response must not be served"
        );
        assert_eq!(cache.len(), 0, "the expired entry must be dropped on read");
    }

    #[test]
    fn a_full_cache_reclaims_expired_entries_before_evicting_live_ones() {
        // Inserts no longer sweep every time, so the sweep has to happen at the
        // one moment it decides anything: when the cache is full and something
        // is about to be evicted. Without it a dead entry holds a slot and the
        // oldest live entry is dropped to make room for the newcomer.
        //
        // The live entry is inserted first on purpose. If the expired one were
        // also the oldest, plain eviction would remove it anyway and this would
        // prove nothing about the sweep.
        let cache = ResponseCache::new(2, Duration::from_secs(60), false);
        cache.insert(
            [1; 32],
            "tenant-a",
            CacheScope::Private,
            Duration::from_secs(30),
            stored(json!({"r": "oldest-but-live"})),
        );
        cache.insert(
            [2; 32],
            "tenant-a",
            CacheScope::Private,
            Duration::from_millis(20),
            stored(json!({"r": "expiring"})),
        );
        assert_eq!(cache.len(), 2);

        std::thread::sleep(Duration::from_millis(50));

        cache.insert(
            [3; 32],
            "tenant-a",
            CacheScope::Private,
            Duration::from_secs(30),
            stored(json!({"r": "new"})),
        );

        assert_eq!(cache.len(), 2, "the cache must stay within max_entries");
        assert_eq!(
            cache.get([1; 32], "tenant-a").unwrap().body["r"],
            "oldest-but-live",
            "a live entry must not be evicted while a dead one holds a slot"
        );
        assert!(cache.get([2; 32], "tenant-a").is_none());
        assert_eq!(cache.get([3; 32], "tenant-a").unwrap().body["r"], "new");
    }

    #[test]
    fn a_full_cache_of_live_entries_evicts_the_oldest() {
        let cache = ResponseCache::new(2, Duration::from_secs(60), false);
        for (slot, label) in [([1; 32], "first"), ([2; 32], "second")] {
            cache.insert(
                slot,
                "tenant-a",
                CacheScope::Private,
                Duration::from_secs(30),
                stored(json!({ "r": label })),
            );
        }
        cache.insert(
            [3; 32],
            "tenant-a",
            CacheScope::Private,
            Duration::from_secs(30),
            stored(json!({"r": "third"})),
        );

        assert_eq!(cache.len(), 2);
        assert!(cache.get([1; 32], "tenant-a").is_none());
        assert_eq!(cache.get([2; 32], "tenant-a").unwrap().body["r"], "second");
        assert_eq!(cache.get([3; 32], "tenant-a").unwrap().body["r"], "third");
    }

    #[test]
    fn rendering_substitutes_the_callers_id_without_touching_the_shared_body() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete"}});

        let rendered = render_with_id(&body, Some(&json!("caller-7")));
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["id"], json!("caller-7"));
        assert_eq!(parsed["result"]["resultType"], "complete");
        assert_eq!(body["id"], json!(1), "the cached body must be untouched");

        // A stored body with no id still gets the caller's.
        let idless = json!({"jsonrpc": "2.0", "result": {}});
        let rendered = render_with_id(&idless, Some(&json!(9)));
        assert_eq!(
            serde_json::from_str::<Value>(&rendered).unwrap()["id"],
            json!(9)
        );

        // No caller id means the stored body is rendered as it stands.
        assert_eq!(
            serde_json::from_str::<Value>(&render_with_id(&body, None)).unwrap(),
            body
        );
    }

    #[test]
    fn a_lookup_does_not_disturb_other_live_entries() {
        // The old sweep touched every key on every read. Nothing else may be
        // evicted now that lookups only examine the two keys they need.
        let cache = ResponseCache::new(16, Duration::from_secs(60), false);
        put(&cache, "tenant-a", CacheScope::Private, json!({"r": "a"}));
        cache.insert(
            [9; 32],
            "tenant-b",
            CacheScope::Private,
            Duration::from_secs(30),
            stored(json!({"r": "b"})),
        );

        assert!(cache.get([1; 32], "tenant-c").is_none());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get([7; 32], "tenant-a").unwrap().body["r"], "a");
        assert_eq!(cache.get([9; 32], "tenant-b").unwrap().body["r"], "b");
    }

    #[test]
    fn public_results_do_not_cross_authorization_contexts_by_default() {
        let cache = ResponseCache::new(16, Duration::from_secs(60), false);
        put(&cache, "tenant-a", CacheScope::Public, json!({"r": "a"}));

        assert!(cache.get([7; 32], "tenant-a").is_some());
        assert!(
            cache.get([7; 32], "tenant-b").is_none(),
            "an origin declaring `public` must not leak across tenants unless the operator opted in"
        );
    }

    #[test]
    fn public_results_are_shared_once_the_operator_opts_in() {
        let cache = ResponseCache::new(16, Duration::from_secs(60), true);
        put(&cache, "tenant-a", CacheScope::Public, json!({"r": "a"}));

        assert_eq!(cache.get([7; 32], "tenant-b").unwrap().body["r"], "a");
    }

    #[test]
    fn private_results_stay_isolated_regardless_of_the_sharing_switch() {
        for share_public in [false, true] {
            let cache = ResponseCache::new(16, Duration::from_secs(60), share_public);
            put(&cache, "tenant-a", CacheScope::Private, json!({"r": "a"}));
            assert!(cache.get([7; 32], "tenant-a").is_some());
            assert!(cache.get([7; 32], "tenant-b").is_none());
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
        assert!(cache.get([7; 32], "tenant-b").is_some());

        put(
            &cache,
            "tenant-a",
            CacheScope::Private,
            json!({"r": "private"}),
        );
        assert!(
            cache.get([7; 32], "tenant-b").is_none(),
            "the superseded shared entry must not outlive its private replacement"
        );
        assert_eq!(cache.get([7; 32], "tenant-a").unwrap().body["r"], "private");
    }

    #[test]
    fn a_demoted_public_result_does_not_purge_other_tenants() {
        // With sharing off, one tenant's `public` result is stored privately and
        // must not evict the entry another tenant already holds.
        let cache = ResponseCache::new(16, Duration::from_secs(60), false);
        put(&cache, "tenant-a", CacheScope::Private, json!({"r": "a"}));
        put(&cache, "tenant-b", CacheScope::Public, json!({"r": "b"}));

        assert_eq!(cache.get([7; 32], "tenant-a").unwrap().body["r"], "a");
        assert_eq!(cache.get([7; 32], "tenant-b").unwrap().body["r"], "b");
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
