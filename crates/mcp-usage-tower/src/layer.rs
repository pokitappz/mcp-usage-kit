//! Generic Tower [`Layer`](tower::Layer) implementation.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use mcp_usage_core::{Call, Charge, Method, ResultType, TaskStatus, decide_with_task_origin};
use mcp_usage_export::{NoopRecorder, RecordOutcome, SharedRecorder, UsageEvent};
use serde_json::{Value, json};
use tower::{Layer, Service};

use crate::auth::{AuthFailureLimit, Tenant, TenantStore, hash_api_key};
use crate::cache::{CachedResponse, RequestMetadata, ResponseCache, cache_hints, inspect_request};
use crate::classify::classify_protocol_headers;
use crate::deferred::DeferredCompletions;
use crate::metrics::EdgeMetrics;
use crate::task::{InMemoryTaskStore, TaskAttributionStore};
use crate::{METHOD_HEADER, NAME_HEADER, PROTOCOL_VERSION_HEADER};

/// Erased body error returned by [`MeterService`].
pub type BoxError = Box<dyn StdError + Send + Sync>;
/// Response body returned by [`MeterService`].
pub type MeterBody = UnsyncBoxBody<Bytes, BoxError>;

const DEFAULT_CACHE_ENTRIES: usize = 512;
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Terminal accounting parked for want of somewhere to await.
const DEFAULT_DEFERRED_CAPACITY: usize = 4_096;
/// Parked completions run at the start of each request.
///
/// Small on purpose. This borrows the request path to make progress, so it has
/// to stay cheap; an application under load has no shortage of requests to
/// amortize the backlog across.
const DEFAULT_DEFERRED_DRAIN_PER_REQUEST: usize = 2;

/// Runtime configuration shared by every cloned metering service.
#[derive(Clone)]
pub struct EdgeConfig {
    tenants: Arc<dyn TenantStore>,
    recorder: SharedRecorder,
    tasks: Arc<dyn TaskAttributionStore>,
    cache: Arc<ResponseCache>,
    metrics: Arc<EdgeMetrics>,
    meter_name: String,
    max_request_body: usize,
    max_response_capture: usize,
    cache_max_entries: usize,
    cache_max_ttl: Duration,
    share_public_cache: bool,
    forward_credentials: bool,
    deferred: Arc<DeferredCompletions>,
    deferred_drain_per_request: usize,
    auth_failure_limit: Option<Arc<AuthFailureLimit>>,
}

impl std::fmt::Debug for EdgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeConfig")
            .field("meter_name", &self.meter_name)
            .field("max_request_body", &self.max_request_body)
            .field("max_response_capture", &self.max_response_capture)
            .field("share_public_cache", &self.share_public_cache)
            .field("forward_credentials", &self.forward_credentials)
            .field("deferred_pending", &self.deferred.len())
            .finish_non_exhaustive()
    }
}

impl EdgeConfig {
    /// Construct an edge using API-key tenants and a no-op billing recorder.
    #[must_use]
    pub fn new(tenants: Arc<dyn TenantStore>) -> Self {
        Self {
            tenants,
            recorder: Arc::new(NoopRecorder),
            tasks: Arc::new(InMemoryTaskStore::new()),
            cache: Arc::new(ResponseCache::new(
                DEFAULT_CACHE_ENTRIES,
                DEFAULT_CACHE_TTL,
                false,
            )),
            metrics: Arc::new(EdgeMetrics::default()),
            meter_name: "mcp_units".to_owned(),
            max_request_body: 1024 * 1024,
            max_response_capture: 1024 * 1024,
            cache_max_entries: DEFAULT_CACHE_ENTRIES,
            cache_max_ttl: DEFAULT_CACHE_TTL,
            share_public_cache: false,
            forward_credentials: false,
            deferred: Arc::new(DeferredCompletions::new(DEFAULT_DEFERRED_CAPACITY)),
            deferred_drain_per_request: DEFAULT_DEFERRED_DRAIN_PER_REQUEST,
            auth_failure_limit: None,
        }
    }

    /// Rebuild the cache from every input that shapes it.
    ///
    /// The builders below are order-independent because each one records its
    /// setting and then rebuilds from all of them. Constructing the cache in
    /// place inside a single builder would let a later call silently discard an
    /// earlier one.
    fn rebuild_cache(&mut self) {
        self.cache = Arc::new(ResponseCache::new(
            self.cache_max_entries,
            self.cache_max_ttl,
            self.share_public_cache,
        ));
    }

    /// Install the hot-path usage recorder.
    #[must_use]
    pub fn with_recorder(mut self, recorder: SharedRecorder) -> Self {
        self.recorder = recorder;
        self
    }

    /// Install a durable task-attribution store.
    #[must_use]
    pub fn with_task_store(mut self, tasks: Arc<dyn TaskAttributionStore>) -> Self {
        self.tasks = tasks;
        self
    }

    /// Set the provider meter event name.
    #[must_use]
    pub fn with_meter_name(mut self, meter_name: impl Into<String>) -> Self {
        self.meter_name = meter_name.into();
        self
    }

    /// Configure cache capacity and the maximum honored server TTL.
    #[must_use]
    pub fn with_cache(mut self, max_entries: usize, max_ttl: Duration) -> Self {
        self.cache_max_entries = max_entries;
        self.cache_max_ttl = max_ttl;
        self.rebuild_cache();
        self
    }

    /// Share results the origin marks `cacheScope: "public"` across tenants.
    ///
    /// Disabled by default. A `"public"` result is an assertion by the origin
    /// that the body does not depend on who asked for it; honoring it puts one
    /// entry in a bucket every authorization context reads, so an origin that
    /// mislabels a tenant-specific result turns that mistake into a cross-tenant
    /// disclosure at the edge. Cacheable methods include `resources/read`, so
    /// the blast radius is resource contents, not just discovery listings.
    ///
    /// Enable this only when the origin's public results are genuinely
    /// tenant-independent. Leaving it off costs cache hit rate and nothing else:
    /// a cache may always decline to reuse a response.
    #[must_use]
    pub fn with_public_cache_sharing(mut self, share: bool) -> Self {
        self.share_public_cache = share;
        self.rebuild_cache();
        self
    }

    /// Forward the API key to the inner service after authenticating it.
    ///
    /// Disabled by default: the edge consumes the credential, so passing it on
    /// hands the inner service a secret it was not issued. That is harmless when
    /// the origin runs in-process, and an exposure the moment this layer fronts
    /// an origin in another trust domain.
    ///
    /// Enable it when the origin performs its own check against the same key.
    #[must_use]
    pub fn with_credential_forwarding(mut self, forward: bool) -> Self {
        self.forward_credentials = forward;
        self
    }

    /// Bound bytes accepted from one MCP request body.
    #[must_use]
    pub fn with_max_request_body(mut self, bytes: usize) -> Self {
        self.max_request_body = bytes;
        self
    }

    /// Bound bytes retained while observing a response. Oversized responses
    /// continue streaming but fail toward not charging and not caching.
    #[must_use]
    pub fn with_max_response_capture(mut self, bytes: usize) -> Self {
        self.max_response_capture = bytes;
        self
    }

    /// Shared edge counters.
    #[must_use]
    pub fn metrics(&self) -> Arc<EdgeMetrics> {
        self.metrics.clone()
    }

    /// Terminal accounting parked because it could not finish without awaiting.
    ///
    /// Only a durable task store parks anything: its futures perform real I/O,
    /// and the body is released from `Drop`, where nothing may await. Every
    /// subsequent request runs a bounded number of them, so an application that
    /// ignores this still converges. Draining explicitly is better, and draining
    /// on shutdown is what stops a departing process from taking durable-task
    /// charges with it.
    ///
    /// Take the handle before passing the configuration to the layer.
    #[must_use]
    pub fn deferred(&self) -> Arc<DeferredCompletions> {
        self.deferred.clone()
    }

    /// Bound how much parked accounting each request runs.
    ///
    /// Zero leaves draining entirely to [`EdgeConfig::deferred`]. Raising it
    /// clears a backlog sooner at the cost of latency on the requests that do
    /// the work.
    #[must_use]
    pub fn with_deferred_drain_per_request(mut self, completions: usize) -> Self {
        self.deferred_drain_per_request = completions;
        self
    }

    /// Bound how many completions may be parked before the oldest is discarded.
    #[must_use]
    pub fn with_deferred_capacity(mut self, capacity: usize) -> Self {
        self.deferred = Arc::new(DeferredCompletions::new(capacity));
        self
    }

    /// Refuse further guessing after `max_failures` bad credentials in `window`.
    ///
    /// Disabled by default, because the useful ceiling depends entirely on how
    /// many clients an edge serves. Only failures consume the budget, so a
    /// caller with a valid key is never affected: exhausting it turns a wrong
    /// key's `401` into a `429` and nothing more.
    ///
    /// This bounds sustained guessing across the whole edge. It is not
    /// per-client limiting, which needs a client identity this layer cannot
    /// trust, and belongs in the proxy in front of it.
    #[must_use]
    pub fn with_auth_failure_limit(mut self, max_failures: u64, window: Duration) -> Self {
        self.auth_failure_limit = Some(Arc::new(AuthFailureLimit::new(max_failures, window)));
        self
    }
}

/// Tower layer that installs [`MeterService`].
#[derive(Debug, Clone)]
pub struct MeterLayer {
    config: Arc<EdgeConfig>,
}

impl MeterLayer {
    /// Construct a metering layer from its shared configuration.
    #[must_use]
    pub fn new(config: EdgeConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl<S> Layer<S> for MeterLayer {
    type Service = MeterService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MeterService {
            inner,
            config: self.config.clone(),
        }
    }
}

/// Service produced by [`MeterLayer`].
#[derive(Debug, Clone)]
pub struct MeterService<S> {
    inner: S,
    config: Arc<EdgeConfig>,
}

impl<S, RequestBody, ResponseBody> Service<Request<RequestBody>> for MeterService<S>
where
    RequestBody: Body<Data = Bytes> + Send + 'static,
    S: Service<Request<Full<Bytes>>, Response = Response<ResponseBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResponseBody: Body<Data = Bytes> + Send + 'static,
    ResponseBody::Error: StdError + Send + Sync + 'static,
{
    type Response = Response<MeterBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the request lifecycle stays linear so security-gate ordering is auditable"
    )]
    fn call(&mut self, request: Request<RequestBody>) -> Self::Future {
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let config = self.config.clone();

        Box::pin(async move {
            // Borrow this request to make progress on accounting a previous one
            // could not finish. Bounded, so a backlog is amortized across
            // traffic rather than charged to whichever request finds it.
            if config.deferred_drain_per_request > 0 {
                config
                    .deferred
                    .drain_some(config.deferred_drain_per_request)
                    .await;
            }

            let (mut parts, request_body) = request.into_parts();
            let protocol = match classify_request_headers(&parts.headers) {
                Ok(protocol) => protocol,
                Err(message) => {
                    config.metrics.rejected();
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        "INVALID_MCP_HEADERS",
                        &message,
                    ));
                }
            };
            let api_key = match extract_api_key(&parts.headers) {
                Ok(api_key) => api_key,
                Err(error) => {
                    let message = match error {
                        CredentialError::Missing => "missing API key",
                        CredentialError::Invalid => "invalid or ambiguous API key headers",
                    };
                    return Ok(refuse_credential(&config, message));
                }
            };
            let Some(tenant) = config.tenants.authenticate(api_key) else {
                return Ok(refuse_credential(&config, "invalid API key"));
            };
            let authorization_context = hash_api_key(api_key);
            config.metrics.classified();

            let request_bytes =
                match collect_request_body(request_body, config.max_request_body).await {
                    Ok(bytes) => bytes,
                    Err(RequestBodyError::TooLarge) => {
                        config.metrics.rejected();
                        return Ok(error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "BODY_TOO_LARGE",
                            "MCP request body exceeds the configured limit",
                        ));
                    }
                    Err(RequestBodyError::Read) => {
                        config.metrics.rejected();
                        return Ok(error_response(
                            StatusCode::BAD_REQUEST,
                            "INVALID_BODY",
                            "failed to read MCP request body",
                        ));
                    }
                };
            let metadata = inspect_request(&protocol.call, &request_bytes);
            if let Some(key) = metadata.cache_key
                && let Some(cached) =
                    config
                        .cache
                        .get(key, &authorization_context, metadata.request_id.as_ref())
            {
                config.metrics.cache_hit();
                let mut cached_request = metadata;
                cached_request.cache_key = None;
                let content_type = cached
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let completion = Completion {
                    config,
                    tenant,
                    authorization_context,
                    call: protocol.call,
                    request: cached_request,
                    response_status: cached.status,
                    response_version: cached.version,
                    response_headers: cached.headers.clone(),
                    content_type,
                };
                return Ok(cached_response(cached, completion));
            }
            if metadata.cache_key.is_some() {
                config.metrics.cache_miss();
            }

            // The credential was consumed by the gate above. Removing both
            // headers unconditionally is safe: `extract_api_key` rejects a
            // request carrying both, so at most one of them is present.
            if !config.forward_credentials {
                parts.headers.remove(crate::API_KEY_HEADER);
                parts.headers.remove(AUTHORIZATION);
            }
            let request = Request::from_parts(parts, Full::new(request_bytes));
            let response = inner.call(request).await?;
            let (parts, body) = response.into_parts();
            let completion = Completion {
                config,
                tenant,
                authorization_context,
                call: protocol.call,
                request: metadata,
                response_status: parts.status,
                response_version: parts.version,
                response_headers: cacheable_headers(&parts.headers),
                content_type: parts
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
            };
            let observed = ObservedBody::new(body, completion);
            let boxed = observed
                .map_err(|error| -> BoxError { Box::new(error) })
                .boxed_unsync();
            Ok(Response::from_parts(parts, boxed))
        })
    }
}

struct ClassifiedCall {
    call: Call,
}

fn classify_request_headers(headers: &HeaderMap) -> Result<ClassifiedCall, String> {
    let version = optional_header_str(headers, PROTOCOL_VERSION_HEADER)?;
    let method = optional_header_str(headers, METHOD_HEADER)?;
    let name = optional_header_str(headers, NAME_HEADER)?;
    let classified = classify_protocol_headers(version, method, name).map_err(|e| e.to_string())?;
    Ok(ClassifiedCall {
        call: Call::new(
            classified.method,
            classified.name.map(std::borrow::Cow::into_owned),
        ),
    })
}

fn optional_header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, String> {
    unique_header(headers, name)?
        .map(|value| {
            value
                .to_str()
                .map_err(|_| format!("{name} is not a visible ASCII header value"))
        })
        .transpose()
}

fn unique_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> Result<Option<&'a http::HeaderValue>, String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(format!("multiple {name} headers are not allowed"));
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialError {
    Missing,
    Invalid,
}

fn extract_api_key(headers: &HeaderMap) -> Result<&str, CredentialError> {
    let direct =
        unique_header(headers, crate::API_KEY_HEADER).map_err(|_| CredentialError::Invalid)?;
    let authorization =
        unique_header(headers, AUTHORIZATION.as_str()).map_err(|_| CredentialError::Invalid)?;
    let value = match (direct, authorization) {
        (None, None) => return Err(CredentialError::Missing),
        (Some(_), Some(_)) => return Err(CredentialError::Invalid),
        (Some(value), None) => value.to_str().map_err(|_| CredentialError::Invalid)?,
        (None, Some(value)) => value
            .to_str()
            .map_err(|_| CredentialError::Invalid)?
            .strip_prefix("Bearer ")
            .ok_or(CredentialError::Invalid)?,
    };
    if value.is_empty() {
        return Err(CredentialError::Invalid);
    }
    Ok(value)
}

/// Refuse a request whose credential did not authenticate.
///
/// The failure budget is consulted only here, on the path a caller reaches by
/// presenting a credential that did not work. A valid key never passes through
/// this function, which is what makes a global budget safe: guessing cannot
/// throttle callers who are not guessing.
fn refuse_credential(config: &EdgeConfig, message: &str) -> Response<MeterBody> {
    if let Some(limit) = config.auth_failure_limit.as_ref() {
        if limit.is_exhausted() {
            config.metrics.throttled();
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "TOO_MANY_FAILED_CREDENTIALS",
                "too many failed authentication attempts",
            );
        }
        limit.record_failure();
    }
    config.metrics.unauthenticated();
    error_response(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED", message)
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response<MeterBody> {
    let bytes = Bytes::from(json!({"error":{"code":code,"message":message}}).to_string());
    let mut response = Response::new(
        Full::new(bytes)
            .map_err(|never: Infallible| -> BoxError { match never {} })
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    // The message can echo a rejected header value back to the caller, so pin
    // the interpretation to JSON rather than leaving it to content sniffing.
    headers.insert(
        X_CONTENT_TYPE_OPTIONS,
        http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(CACHE_CONTROL, http::HeaderValue::from_static("no-store"));
    response
}

fn cached_response(cached: CachedResponse, completion: Completion) -> Response<MeterBody> {
    let body = Bytes::from(cached.body.to_string());
    let shares_public = completion.config.share_public_cache;
    let observed = ObservedBody::new(Full::new(body), completion);
    let mut response = Response::new(
        observed
            .map_err(|never: Infallible| -> BoxError { match never {} })
            .boxed_unsync(),
    );
    *response.status_mut() = cached.status;
    *response.version_mut() = cached.version;
    *response.headers_mut() = cached.headers;
    let headers = response.headers_mut();
    headers.insert("x-mcp-usage-cache", http::HeaderValue::from_static("hit"));
    headers.insert(
        X_CONTENT_TYPE_OPTIONS,
        http::HeaderValue::from_static("nosniff"),
    );
    if !shares_public {
        // Nothing here is safe for a shared cache to reuse across callers, and a
        // CDN in front of the edge has no idea the body is keyed on an API key.
        headers.insert(CACHE_CONTROL, http::HeaderValue::from_static("private"));
    }
    response
}

fn cacheable_headers(headers: &HeaderMap) -> HeaderMap {
    let mut kept = HeaderMap::new();
    if let Some(content_type) = headers.get(CONTENT_TYPE) {
        kept.insert(CONTENT_TYPE, content_type.clone());
    }
    kept
}

struct Completion {
    config: Arc<EdgeConfig>,
    tenant: Tenant,
    authorization_context: String,
    call: Call,
    request: RequestMetadata,
    response_status: StatusCode,
    response_version: http::Version,
    response_headers: HeaderMap,
    content_type: String,
}

impl Completion {
    #[expect(
        clippy::too_many_lines,
        reason = "terminal accounting stays linear so fail-free ordering remains auditable"
    )]
    async fn finish(self, bytes: Bytes) {
        let Some(body) = terminal_response(&self.content_type, &bytes) else {
            self.config.metrics.unrecognized_response();
            return;
        };
        let response = mcp_usage_core::peek::response(&body);

        let response_task_id = response
            .task
            .as_ref()
            .and_then(|task| task.task_id.as_deref());
        if matches!(self.call.method, Method::TasksGet)
            && let (Some(requested), Some(returned)) =
                (self.request.requested_task_id.as_deref(), response_task_id)
            && requested != returned
        {
            // Never attribute one task's result to a differently requested
            // task. A conforming origin cannot produce this mismatch.
            self.config.metrics.unrecognized_response();
            return;
        }

        if matches!(response.result_type, ResultType::Task)
            && let Some(task_id) = response
                .task
                .as_ref()
                .and_then(|task| task.task_id.as_deref())
            && let Err(error) = self
                .config
                .tasks
                .insert(&self.tenant.id, task_id, self.call.clone())
                .await
        {
            self.config.metrics.record_failure();
            tracing::error!(error = %error, "failed to retain MCP task attribution");
        }

        let response_task_id = response_task_id.or(self.request.requested_task_id.as_deref());
        let terminal_task = response.task.as_ref().is_some_and(|task| {
            matches!(
                task.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        });
        let completed_task = response
            .task
            .as_ref()
            .is_some_and(|task| matches!(task.status, TaskStatus::Completed));
        let task_origin = if matches!(self.call.method, Method::TasksGet) && completed_task {
            if let Some(task_id) = response_task_id {
                match self.config.tasks.claim(&self.tenant.id, task_id).await {
                    Ok(origin) => origin,
                    Err(error) => {
                        self.config.metrics.record_failure();
                        tracing::error!(error = %error, "failed to claim MCP task attribution");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let charge = decide_with_task_origin(
            &self.call,
            &response,
            &self.tenant.prices,
            task_origin.as_ref(),
        );
        match charge {
            Charge::Billable(billable) => {
                let usage = UsageEvent::now(
                    &self.tenant.id,
                    &self.tenant.billing_customer_id,
                    &self.config.meter_name,
                    billable.units,
                    billable.idempotency_key,
                );
                match self.config.recorder.record(usage) {
                    Ok(RecordOutcome::Recorded | RecordOutcome::ZeroUnits) => {
                        self.config.metrics.billed(billable.units);
                    }
                    Ok(RecordOutcome::Duplicate) => {
                        self.config.metrics.duplicate();
                    }
                    Err(error) => {
                        self.config.metrics.record_failure();
                        tracing::error!(error = %error, "failed to buffer MCP usage");
                    }
                }
            }
            Charge::Free(_) => {
                self.config.metrics.free();
                if terminal_task
                    && !completed_task
                    && let Some(task_id) = response_task_id
                    && let Err(error) = self.config.tasks.remove(&self.tenant.id, task_id).await
                {
                    self.config.metrics.record_failure();
                    tracing::error!(error = %error, "failed to remove MCP task attribution");
                }
            }
        }

        if self.response_status.is_success()
            && !self.request.is_continuation
            && has_media_type(&self.content_type, "application/json")
            && let Some(key) = self.request.cache_key
            && let Some((ttl, scope)) = cache_hints(&body)
        {
            self.config.cache.insert(
                key,
                &self.authorization_context,
                scope,
                ttl,
                CachedResponse {
                    status: self.response_status,
                    version: self.response_version,
                    headers: self.response_headers,
                    body,
                },
            );
        }
    }
}

struct ObservedBody<B> {
    inner: Pin<Box<B>>,
    completion: Option<Completion>,
    finishing: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    captured: BytesMut,
    overflowed: bool,
    metrics: Arc<EdgeMetrics>,
    deferred: Arc<DeferredCompletions>,
}

impl<B> ObservedBody<B> {
    fn new(inner: B, completion: Completion) -> Self {
        let metrics = completion.config.metrics.clone();
        let deferred = completion.config.deferred.clone();
        Self {
            inner: Box::pin(inner),
            completion: Some(completion),
            finishing: None,
            captured: BytesMut::new(),
            overflowed: false,
            metrics,
            deferred,
        }
    }

    /// Build the terminal-accounting future, consuming the pending completion.
    ///
    /// Returns `None` when there is nothing left to account for, either because
    /// accounting already ran or because the captured body overflowed.
    fn start_completion(&mut self) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        let completion = self.completion.take()?;
        if self.overflowed {
            completion.config.metrics.unrecognized_response();
            return None;
        }
        let captured = std::mem::take(&mut self.captured).freeze();
        Some(Box::pin(completion.finish(captured)))
    }
}

impl<B> Drop for ObservedBody<B> {
    /// Account for a response the transport stopped polling before end-of-stream.
    ///
    /// A body that declares a `Content-Length` is not polled again once that
    /// many bytes have been written, so `poll_frame` never observes the
    /// end-of-stream that terminal accounting used to depend on. That is the
    /// ordinary case for a fixed-length JSON result, and without this the meter
    /// records nothing at all. Dropping the body is the one signal every
    /// transport does give us.
    ///
    /// Nothing here may await, so the future is polled once with a no-op waker.
    /// That settles it outright for every synchronous recorder and for the
    /// in-process task store, whose futures never yield.
    ///
    /// A durable store performs real I/O, so its first poll pends. That work is
    /// parked on [`DeferredCompletions`] to be driven from somewhere that can
    /// await, because the paths that pend are the ones carrying durable-task
    /// attribution: abandoning them loses the charge for every durable task.
    fn drop(&mut self) {
        let mut context = Context::from_waker(std::task::Waker::noop());
        let unfinished = if let Some(mut finishing) = self.finishing.take() {
            finishing
                .as_mut()
                .poll(&mut context)
                .is_pending()
                .then_some(finishing)
        } else if let Some(mut finishing) = self.start_completion() {
            finishing
                .as_mut()
                .poll(&mut context)
                .is_pending()
                .then_some(finishing)
        } else {
            None
        };
        if let Some(finishing) = unfinished {
            self.metrics.deferred();
            if !self.deferred.park(finishing) {
                // The queue was full, so the oldest accounting was discarded.
                self.metrics.record_failure();
            }
        }
    }
}

impl<B> Body for ObservedBody<B>
where
    B: Body<Data = Bytes>,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(finishing) = self.finishing.as_mut() {
            return match finishing.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.finishing = None;
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            };
        }
        match self.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref()
                    && !self.overflowed
                {
                    let limit = self
                        .completion
                        .as_ref()
                        .map_or(0, |completion| completion.config.max_response_capture);
                    if self.captured.len().saturating_add(data.remaining()) <= limit {
                        self.captured.extend_from_slice(data.chunk());
                    } else {
                        self.captured.clear();
                        self.overflowed = true;
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                if let Some(mut finishing) = self.start_completion() {
                    return match finishing.as_mut().poll(cx) {
                        Poll::Ready(()) => Poll::Ready(None),
                        Poll::Pending => {
                            self.finishing = Some(finishing);
                            Poll::Pending
                        }
                    };
                }
                Poll::Ready(None)
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

pub(crate) fn terminal_response(content_type: &str, bytes: &[u8]) -> Option<Value> {
    if has_media_type(content_type, "application/json") {
        return serde_json::from_slice(bytes).ok();
    }
    if !has_media_type(content_type, "text/event-stream") {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?.replace("\r\n", "\n");
    let mut terminal = None;
    for event in text.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data)
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            terminal = Some(value);
        }
    }
    terminal
}

fn has_media_type(content_type: &str, expected: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(expected))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestBodyError {
    TooLarge,
    Read,
}

async fn collect_request_body<B>(body: B, limit: usize) -> Result<Bytes, RequestBodyError>
where
    B: Body<Data = Bytes>,
{
    let initial_capacity = usize::try_from(body.size_hint().lower())
        .unwrap_or(limit)
        .min(limit);
    let mut captured = BytesMut::with_capacity(initial_capacity);
    let mut body = Box::pin(body);
    while let Some(frame) = body.as_mut().frame().await {
        let frame = frame.map_err(|_| RequestBodyError::Read)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let Some(next_len) = captured.len().checked_add(data.len()) else {
            return Err(RequestBodyError::TooLarge);
        };
        if next_len > limit {
            return Err(RequestBodyError::TooLarge);
        }
        captured.extend_from_slice(&data);
    }
    Ok(captured.freeze())
}

/// Internals reachable from the crate's property tests.
#[cfg(test)]
pub(crate) mod testing {
    pub(crate) use super::terminal_response;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryTenantStore, Tenant};
    use mcp_usage_core::PriceBook;
    use mcp_usage_export::{BillingPipeline, LogExporter};
    use tower::{ServiceBuilder, ServiceExt, service_fn};

    fn request(method: &str, name: Option<&str>, body: &Value, key: &str) -> Request<Full<Bytes>> {
        let mut body = body.clone();
        if let Some(object) = body.as_object_mut() {
            object
                .entry("jsonrpc")
                .or_insert_with(|| Value::String("2.0".to_owned()));
            object
                .entry("method")
                .or_insert_with(|| Value::String(method.to_owned()));
        }
        let mut builder = Request::builder()
            .header(PROTOCOL_VERSION_HEADER, "2026-07-28")
            .header(METHOD_HEADER, method)
            .header(crate::API_KEY_HEADER, key);
        if let Some(name) = name {
            builder = builder.header(NAME_HEADER, name);
        }
        builder
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap()
    }

    fn response(body: &Value) -> Response<Full<Bytes>> {
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
            .unwrap()
    }

    async fn consume(response: Response<MeterBody>) -> Bytes {
        response.into_body().collect().await.unwrap().to_bytes()
    }

    /// Release a body the way a transport does once `Content-Length` is met:
    /// take the data, then drop without ever polling to end-of-stream.
    fn release_without_end_of_stream(response: Response<MeterBody>) {
        let mut body = Box::pin(response.into_body());
        let mut context = Context::from_waker(std::task::Waker::noop());
        let _ = body.as_mut().poll_frame(&mut context);
        drop(body);
    }

    /// A future that pends exactly once, the way a store doing I/O behaves.
    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// A task store that yields before answering, standing in for Redis or
    /// `PostgreSQL` without requiring either to be running.
    #[derive(Debug, Default)]
    struct YieldingTaskStore(InMemoryTaskStore);

    impl TaskAttributionStore for YieldingTaskStore {
        fn insert<'a>(
            &'a self,
            tenant_id: &'a str,
            task_id: &'a str,
            call: Call,
        ) -> crate::TaskStoreFuture<'a, ()> {
            Box::pin(async move {
                YieldOnce(false).await;
                self.0.insert(tenant_id, task_id, call).await
            })
        }
        fn get<'a>(
            &'a self,
            tenant_id: &'a str,
            task_id: &'a str,
        ) -> crate::TaskStoreFuture<'a, Option<Call>> {
            Box::pin(async move {
                YieldOnce(false).await;
                self.0.get(tenant_id, task_id).await
            })
        }
        fn claim<'a>(
            &'a self,
            tenant_id: &'a str,
            task_id: &'a str,
        ) -> crate::TaskStoreFuture<'a, Option<Call>> {
            Box::pin(async move {
                YieldOnce(false).await;
                self.0.claim(tenant_id, task_id).await
            })
        }
        fn remove<'a>(
            &'a self,
            tenant_id: &'a str,
            task_id: &'a str,
        ) -> crate::TaskStoreFuture<'a, ()> {
            Box::pin(async move {
                YieldOnce(false).await;
                self.0.remove(tenant_id, task_id).await
            })
        }
    }

    /// Drive a `tools/call` that creates a task, then a `tasks/get` that reports
    /// it completed, releasing both bodies the way a real transport does.
    async fn run_task_lifecycle<S>(service: S)
    where
        S: tower::Service<Request<Full<Bytes>>, Response = Response<MeterBody>, Error = Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        release_without_end_of_stream(
            service
                .clone()
                .oneshot(request(
                    "tools/call",
                    Some("long_job"),
                    &json!({"id":1,"params":{"name":"long_job"}}),
                    "secret",
                ))
                .await
                .unwrap(),
        );
        release_without_end_of_stream(
            service
                .oneshot(request(
                    "tasks/get",
                    None,
                    &json!({"id":2,"params":{"taskId":"task-1"}}),
                    "secret",
                ))
                .await
                .unwrap(),
        );
    }

    fn task_lifecycle_service(
        config: EdgeConfig,
    ) -> impl tower::Service<
        Request<Full<Bytes>>,
        Response = Response<MeterBody>,
        Error = Infallible,
        Future = impl Send,
    > + Clone
    + Send
    + 'static {
        let stage = Arc::new(std::sync::atomic::AtomicU64::new(0));
        ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                let n = stage.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async move {
                    let result = if n == 0 {
                        json!({"resultType":"task","taskId":"task-1","status":"working"})
                    } else {
                        json!({"resultType":"complete","taskId":"task-1","status":"completed","result":{}})
                    };
                    Ok::<_, Infallible>(response(&json!({"jsonrpc":"2.0","id":n,"result":result})))
                }
            }))
    }

    fn task_tenants() -> Arc<InMemoryTenantStore> {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme")
                .with_prices(PriceBook::flat(1).with_name("long_job", 50)),
        );
        tenants
    }

    #[tokio::test]
    async fn a_store_that_yields_is_parked_rather_than_abandoned() {
        // A durable store performs I/O, so its future pends on the single poll
        // `Drop` can give it. Before this was parked, every durable-task charge
        // was lost: the attribution insert never landed, so the completing poll
        // had nothing to price.
        let config = EdgeConfig::new(task_tenants())
            .with_task_store(Arc::new(YieldingTaskStore::default()))
            .with_deferred_drain_per_request(0);
        let metrics = config.metrics();
        let deferred = config.deferred();

        run_task_lifecycle(task_lifecycle_service(config)).await;

        assert_eq!(
            metrics.snapshot().billed,
            0,
            "nothing can be billed while the store has not answered"
        );
        assert!(
            !deferred.is_empty(),
            "the unfinished accounting must be parked"
        );
        assert_eq!(deferred.dropped(), 0);

        // Draining is the context that can await, so this is where it lands.
        assert!(deferred.drain().await > 0);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.billed, 1, "the completed task must be charged");
        assert_eq!(
            snapshot.billed_units, 50,
            "priced from the originating call"
        );
        assert_eq!(snapshot.record_failures, 0);
        assert!(deferred.is_empty());
    }

    #[tokio::test]
    async fn a_later_request_drains_parked_accounting_on_its_own() {
        // An application that never calls `drain` must still converge, so every
        // request runs a bounded amount of parked work.
        let config =
            EdgeConfig::new(task_tenants()).with_task_store(Arc::new(YieldingTaskStore::default()));
        let metrics = config.metrics();
        let deferred = config.deferred();
        let service = task_lifecycle_service(config);

        run_task_lifecycle(service.clone()).await;
        assert_eq!(metrics.snapshot().billed, 0);

        // Ordinary unrelated traffic, with no drain call anywhere.
        for id in 3..8 {
            release_without_end_of_stream(
                service
                    .clone()
                    .oneshot(request(
                        "tools/list",
                        None,
                        &json!({"id":id,"params":{}}),
                        "secret",
                    ))
                    .await
                    .unwrap(),
            );
        }

        assert_eq!(
            metrics.snapshot().billed,
            1,
            "subsequent requests must complete the parked accounting"
        );
        assert_eq!(metrics.snapshot().billed_units, 50);
        assert!(deferred.is_empty());
    }

    #[tokio::test]
    async fn a_synchronous_store_never_parks_anything() {
        // The default in-process store answers without yielding, so the common
        // deployment must not pay for the queue at all.
        let config = EdgeConfig::new(task_tenants());
        let metrics = config.metrics();
        let deferred = config.deferred();

        run_task_lifecycle(task_lifecycle_service(config)).await;

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.billed, 1);
        assert_eq!(snapshot.billed_units, 50);
        assert_eq!(snapshot.deferred, 0, "nothing should have been parked");
        assert!(deferred.is_empty());
    }

    #[tokio::test]
    async fn complete_tool_call_is_recorded_after_body_consumption() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme")
                .with_prices(PriceBook::flat(1).with_name("expensive", 25)),
        );
        let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
        let config = EdgeConfig::new(tenants).with_recorder(billing.clone());
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({
                    "jsonrpc":"2.0","id":1,
                    "result":{"resultType":"complete","content":[]}
                })))
            }));

        let response = service
            .oneshot(request(
                "tools/call",
                Some("expensive"),
                &json!({"jsonrpc":"2.0","id":1,"params":{"name":"expensive"}}),
                "secret",
            ))
            .await
            .unwrap();
        assert_eq!(billing.pending_buckets(), 0, "not billed before delivery");
        consume(response).await;
        assert_eq!(billing.pending_buckets(), 1);
        billing.flush().await.unwrap();
        assert_eq!(billing.exporter().exported()[0].units, 25);
    }

    #[tokio::test]
    async fn private_cache_never_crosses_authorization_contexts_and_rewrites_response_id() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("key-a", Tenant::new("a", "cus_a"));
        // Even two credentials mapped to the same tenant are distinct
        // authorization contexts for private-cache purposes.
        tenants.insert_unchecked("key-b", Tenant::new("a", "cus_a"));
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inner_calls = calls.clone();
        let config = EdgeConfig::new(tenants);
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                inner_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async {
                    Ok::<_, Infallible>(response(&json!({
                        "jsonrpc":"2.0","id":1,
                        "result":{"resultType":"complete","tools":[],"ttlMs":60000,"cacheScope":"private"}
                    })))
                }
            }));

        let first = service
            .clone()
            .oneshot(request(
                "tools/list",
                None,
                &json!({"jsonrpc":"2.0","id":1,"params":{}}),
                "key-a",
            ))
            .await
            .unwrap();
        consume(first).await;

        let hit = service
            .clone()
            .oneshot(request(
                "tools/list",
                None,
                &json!({"jsonrpc":"2.0","id":99,"params":{}}),
                "key-a",
            ))
            .await
            .unwrap();
        assert_eq!(hit.headers()["x-mcp-usage-cache"], "hit");
        let hit_body: Value = serde_json::from_slice(&consume(hit).await).unwrap();
        assert_eq!(hit_body["id"], 99);

        let other = service
            .oneshot(request(
                "tools/list",
                None,
                &json!({"jsonrpc":"2.0","id":2,"params":{}}),
                "key-b",
            ))
            .await
            .unwrap();
        consume(other).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cache_hits_for_billable_resources_are_still_metered_on_delivery() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme")
                .with_prices(PriceBook::flat(1).with_name("file:///report", 4)),
        );
        let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inner_calls = calls.clone();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(
                EdgeConfig::new(tenants).with_recorder(billing.clone()),
            ))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                inner_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async {
                    Ok::<_, Infallible>(response(&json!({
                        "jsonrpc":"2.0","id":1,
                        "result":{
                            "resultType":"complete",
                            "contents":[],
                            "ttlMs":60000,
                            "cacheScope":"private"
                        }
                    })))
                }
            }));

        for id in [1, 2] {
            consume(
                service
                    .clone()
                    .oneshot(request(
                        "resources/read",
                        Some("file:///report"),
                        &json!({"jsonrpc":"2.0","id":id,"params":{"uri":"file:///report"}}),
                        "secret",
                    ))
                    .await
                    .unwrap(),
            )
            .await;
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        billing.flush().await.unwrap();
        assert_eq!(billing.exporter().exported()[0].units, 8);
    }

    #[tokio::test]
    async fn trace_context_reaches_the_rmcp_service_unchanged() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));
        let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner_seen = seen.clone();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(EdgeConfig::new(tenants)))
            .service(service_fn(move |request: Request<Full<Bytes>>| {
                inner_seen.store(
                    request.headers().get("traceparent").is_some_and(|value| {
                        value == "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                    }),
                    std::sync::atomic::Ordering::Relaxed,
                );
                async {
                    Ok::<_, Infallible>(response(&json!({
                        "jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}
                    })))
                }
            }));
        let mut request = request(
            "tools/call",
            Some("tool"),
            &json!({"id":1,"params":{"name":"tool"}}),
            "secret",
        );
        request.headers_mut().insert(
            "traceparent",
            http::HeaderValue::from_static(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
        );
        consume(service.oneshot(request).await.unwrap()).await;
        assert!(seen.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn task_completion_uses_origin_price_and_repeat_poll_is_free() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme")
                .with_prices(PriceBook::flat(1).with_name("long_job", 50)),
        );
        let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
        let stage = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inner_stage = stage.clone();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(
                EdgeConfig::new(tenants).with_recorder(billing.clone()),
            ))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                let n = inner_stage.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async move {
                    let result = if n == 0 {
                        json!({"resultType":"task","taskId":"task-1","status":"working"})
                    } else {
                        json!({"resultType":"complete","taskId":"task-1","status":"completed","result":{}})
                    };
                    Ok::<_, Infallible>(response(&json!({"jsonrpc":"2.0","id":n,"result":result})))
                }
            }));

        consume(
            service
                .clone()
                .oneshot(request(
                    "tools/call",
                    Some("long_job"),
                    &json!({"id":1,"params":{"name":"long_job"}}),
                    "secret",
                ))
                .await
                .unwrap(),
        )
        .await;
        for id in [2, 3] {
            consume(
                service
                    .clone()
                    .oneshot(request(
                        "tasks/get",
                        None,
                        &json!({"id":id,"params":{"taskId":"task-1"}}),
                        "secret",
                    ))
                    .await
                    .unwrap(),
            )
            .await;
        }
        billing.flush().await.unwrap();
        let exported = billing.exporter().exported();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].units, 50);
    }

    #[tokio::test]
    async fn mismatched_task_response_cannot_bill_another_tasks_origin() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme")
                .with_prices(PriceBook::flat(1).with_name("expensive", 100)),
        );
        let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
        let stage = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inner_stage = stage.clone();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(
                EdgeConfig::new(tenants).with_recorder(billing.clone()),
            ))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                let n = inner_stage.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async move {
                    let result = match n {
                        0 => json!({"resultType":"task","taskId":"task-1","status":"working"}),
                        1 => json!({"resultType":"task","taskId":"task-2","status":"working"}),
                        _ => json!({
                            "resultType":"complete",
                            "taskId":"task-2",
                            "status":"completed",
                            "result":{}
                        }),
                    };
                    Ok::<_, Infallible>(response(&json!({"jsonrpc":"2.0","id":n,"result":result})))
                }
            }));

        for (name, id) in [("cheap", 1), ("expensive", 2)] {
            consume(
                service
                    .clone()
                    .oneshot(request(
                        "tools/call",
                        Some(name),
                        &json!({"id":id,"params":{"name":name}}),
                        "secret",
                    ))
                    .await
                    .unwrap(),
            )
            .await;
        }
        consume(
            service
                .oneshot(request(
                    "tasks/get",
                    None,
                    &json!({"id":3,"params":{"taskId":"task-1"}}),
                    "secret",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(billing.flush().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn accounting_runs_when_the_transport_stops_polling_early() {
        // A transport is not obliged to poll a body to end-of-stream. hyper
        // stops as soon as the declared Content-Length has been written, so
        // `poll_frame` never returns `Poll::Ready(None)` for an ordinary
        // fixed-length JSON result. Terminal accounting must survive that:
        // dropping the body is the only signal every transport gives us.
        //
        // This deliberately drives the body by hand rather than through
        // `BodyExt::collect`, which always polls to `None` and would hide the
        // regression this test exists to catch.
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme").with_prices(PriceBook::flat(1).with_name("tool", 7)),
        );
        let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
        let config = EdgeConfig::new(tenants).with_recorder(billing.clone());
        let metrics = config.metrics();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({
                    "jsonrpc":"2.0","id":1,
                    "result":{"resultType":"complete","content":[]}
                })))
            }));

        let response = service
            .oneshot(request(
                "tools/call",
                Some("tool"),
                &json!({"id":1,"params":{"name":"tool"}}),
                "secret",
            ))
            .await
            .unwrap();

        let mut body = Box::pin(response.into_body());
        let mut context = Context::from_waker(std::task::Waker::noop());
        // Take the single data frame and then stop, exactly as a transport does
        // once it has satisfied Content-Length.
        assert!(
            matches!(
                body.as_mut().poll_frame(&mut context),
                Poll::Ready(Some(Ok(_)))
            ),
            "expected one data frame"
        );
        assert_eq!(
            metrics.snapshot().billed,
            0,
            "nothing should be billed before the body is released"
        );

        drop(body);

        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.billed, 1,
            "the delivery must still be accounted for"
        );
        assert_eq!(snapshot.billed_units, 7);
        assert_eq!(snapshot.record_failures, 0);
        assert_eq!(billing.pending_buckets(), 1);
    }

    #[tokio::test]
    async fn a_fully_consumed_body_is_accounted_for_exactly_once() {
        // The guard against the Drop path double-counting what `poll_frame`
        // already recorded.
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked(
            "secret",
            Tenant::new("acme", "cus_acme").with_prices(PriceBook::flat(1).with_name("tool", 5)),
        );
        let config = EdgeConfig::new(tenants);
        let metrics = config.metrics();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({
                    "jsonrpc":"2.0","id":1,
                    "result":{"resultType":"complete","content":[]}
                })))
            }));

        consume(
            service
                .oneshot(request(
                    "tools/call",
                    Some("tool"),
                    &json!({"id":1,"params":{"name":"tool"}}),
                    "secret",
                ))
                .await
                .unwrap(),
        )
        .await;

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.billed, 1);
        assert_eq!(snapshot.billed_units, 5);
    }

    #[tokio::test]
    async fn public_results_cross_tenants_only_when_sharing_is_enabled() {
        // Same scenario as the private-scope test above, but with the origin
        // declaring `cacheScope: "public"`. The origin call count is the
        // observable: two calls means tenant B was served from the origin, one
        // means it was served tenant A's cached body.
        for (share, expected_origin_calls) in [(false, 2), (true, 1)] {
            let tenants = Arc::new(InMemoryTenantStore::new());
            tenants.insert_unchecked("key-a", Tenant::new("a", "cus_a"));
            tenants.insert_unchecked("key-b", Tenant::new("b", "cus_b"));
            let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let inner_calls = calls.clone();
            let service = ServiceBuilder::new()
                .layer(MeterLayer::new(
                    EdgeConfig::new(tenants).with_public_cache_sharing(share),
                ))
                .service(service_fn(move |_request: Request<Full<Bytes>>| {
                    inner_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    async {
                        Ok::<_, Infallible>(response(&json!({
                            "jsonrpc":"2.0","id":1,
                            "result":{
                                "resultType":"complete","tools":[],
                                "ttlMs":60000,"cacheScope":"public"
                            }
                        })))
                    }
                }));

            for key in ["key-a", "key-b"] {
                consume(
                    service
                        .clone()
                        .oneshot(request(
                            "tools/list",
                            None,
                            &json!({"jsonrpc":"2.0","id":1,"params":{}}),
                            key,
                        ))
                        .await
                        .unwrap(),
                )
                .await;
            }

            assert_eq!(
                calls.load(std::sync::atomic::Ordering::Relaxed),
                expected_origin_calls,
                "with_public_cache_sharing({share})"
            );
        }
    }

    #[tokio::test]
    async fn the_consumed_credential_does_not_reach_the_inner_service() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));

        for (forward, expected) in [(false, false), (true, true)] {
            for bearer in [false, true] {
                let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let inner_seen = seen.clone();
                let service = ServiceBuilder::new()
                    .layer(MeterLayer::new(
                        EdgeConfig::new(tenants.clone()).with_credential_forwarding(forward),
                    ))
                    .service(service_fn(move |request: Request<Full<Bytes>>| {
                        let headers = request.headers();
                        inner_seen.store(
                            headers.contains_key(crate::API_KEY_HEADER)
                                || headers.contains_key(AUTHORIZATION),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        async {
                            Ok::<_, Infallible>(response(&json!({
                                "jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}
                            })))
                        }
                    }));

                let mut call = request(
                    "tools/call",
                    Some("tool"),
                    &json!({"id":1,"params":{"name":"tool"}}),
                    "secret",
                );
                if bearer {
                    // Exactly one credential header may be present.
                    call.headers_mut().remove(crate::API_KEY_HEADER);
                    call.headers_mut().insert(
                        AUTHORIZATION,
                        http::HeaderValue::from_static("Bearer secret"),
                    );
                }
                consume(service.oneshot(call).await.unwrap()).await;

                assert_eq!(
                    seen.load(std::sync::atomic::Ordering::Relaxed),
                    expected,
                    "forwarding={forward} bearer={bearer}"
                );
            }
        }
    }

    #[tokio::test]
    async fn generated_responses_pin_their_content_type_and_stay_out_of_shared_caches() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("key-a", Tenant::new("a", "cus_a"));
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(EdgeConfig::new(tenants)))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({
                    "jsonrpc":"2.0","id":1,
                    "result":{"resultType":"complete","tools":[],"ttlMs":60000,"cacheScope":"private"}
                })))
            }));

        // The rejection path echoes the offending header value back to the caller.
        let mut malformed = request("tools/list", None, &json!({"id":1,"params":{}}), "key-a");
        malformed.headers_mut().insert(
            PROTOCOL_VERSION_HEADER,
            http::HeaderValue::from_static("<not-a-date>"),
        );
        let rejected = service.clone().oneshot(malformed).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert_eq!(rejected.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(rejected.headers()[CACHE_CONTROL], "no-store");

        consume(
            service
                .clone()
                .oneshot(request(
                    "tools/list",
                    None,
                    &json!({"id":1,"params":{}}),
                    "key-a",
                ))
                .await
                .unwrap(),
        )
        .await;
        let hit = service
            .oneshot(request(
                "tools/list",
                None,
                &json!({"id":2,"params":{}}),
                "key-a",
            ))
            .await
            .unwrap();
        assert_eq!(hit.headers()["x-mcp-usage-cache"], "hit");
        assert_eq!(hit.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(hit.headers()[CACHE_CONTROL], "private");
        consume(hit).await;
    }

    #[tokio::test]
    async fn sustained_guessing_is_refused_without_affecting_valid_keys() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));
        let config =
            EdgeConfig::new(tenants).with_auth_failure_limit(3, std::time::Duration::from_secs(60));
        let metrics = config.metrics();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({
                    "jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}
                })))
            }));

        let guess = |key: &'static str| {
            let service = service.clone();
            async move {
                service
                    .oneshot(request(
                        "tools/list",
                        None,
                        &json!({"id":1,"params":{}}),
                        key,
                    ))
                    .await
                    .unwrap()
                    .status()
            }
        };

        for _ in 0..3 {
            assert_eq!(guess("wrong").await, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(
            guess("wrong").await,
            StatusCode::TOO_MANY_REQUESTS,
            "sustained guessing must stop being answered"
        );

        // A valid key never consumed the budget, so it is still served.
        let allowed = service
            .clone()
            .oneshot(request(
                "tools/list",
                None,
                &json!({"id":9,"params":{}}),
                "secret",
            ))
            .await
            .unwrap();
        assert_eq!(
            allowed.status(),
            StatusCode::OK,
            "throttling guessers must not lock out legitimate callers"
        );
        consume(allowed).await;

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.unauthenticated, 3);
        assert_eq!(snapshot.throttled, 1);
    }

    #[tokio::test]
    async fn credential_failures_are_counted_apart_from_header_failures() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));
        let config = EdgeConfig::new(tenants);
        let metrics = config.metrics();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(config))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({"result": {}})))
            }));

        let unknown_key = service
            .clone()
            .oneshot(request(
                "tools/list",
                None,
                &json!({"id":1,"params":{}}),
                "wrong",
            ))
            .await
            .unwrap();
        assert_eq!(unknown_key.status(), StatusCode::UNAUTHORIZED);

        // Classification runs before authentication, so this never reaches the
        // credential gate and must land on the other counter.
        let mut legacy = request("tools/list", None, &json!({"id":1,"params":{}}), "secret");
        legacy.headers_mut().insert(
            PROTOCOL_VERSION_HEADER,
            http::HeaderValue::from_static("2025-06-18"),
        );
        assert_eq!(
            service.oneshot(legacy).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.unauthenticated, 1);
        assert_eq!(snapshot.rejected, 1);
    }

    #[test]
    fn extracts_terminal_response_from_sse_without_confusing_notifications() {
        let stream = b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                       : keepalive\n\n\
                       data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"resultType\":\"complete\"}}\n\n";
        let terminal = terminal_response("text/event-stream", stream).unwrap();
        assert_eq!(terminal["id"], 1);
    }

    #[test]
    fn content_type_matching_requires_an_exact_media_type() {
        assert!(has_media_type(
            "Application/JSON; charset=utf-8",
            "application/json"
        ));
        assert!(!has_media_type("application/jsonp", "application/json"));
        assert!(terminal_response("application/jsonp", b"{}").is_none());
    }

    #[tokio::test]
    async fn oversized_request_bodies_are_rejected_before_the_origin() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inner_calls = calls.clone();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(
                EdgeConfig::new(tenants).with_max_request_body(16),
            ))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                inner_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async { Ok::<_, Infallible>(response(&json!({"result": {}}))) }
            }));

        let response = service
            .oneshot(request(
                "tools/call",
                Some("tool"),
                &json!({"id":1,"params":{"name":"tool","payload":"too large"}}),
                "secret",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn ambiguous_or_duplicate_security_headers_are_rejected() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(EdgeConfig::new(tenants)))
            .service(service_fn(|_request: Request<Full<Bytes>>| async {
                Ok::<_, Infallible>(response(&json!({"result": {}})))
            }));

        let mut ambiguous = request("tools/list", None, &json!({"id":1,"params":{}}), "secret");
        ambiguous.headers_mut().insert(
            AUTHORIZATION,
            http::HeaderValue::from_static("Bearer secret"),
        );
        assert_eq!(
            service.clone().oneshot(ambiguous).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let mut duplicate = request("tools/list", None, &json!({"id":1,"params":{}}), "secret");
        duplicate
            .headers_mut()
            .append(METHOD_HEADER, http::HeaderValue::from_static("tools/list"));
        assert_eq!(
            service.oneshot(duplicate).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn a_header_body_mismatch_cannot_use_the_cache() {
        let tenants = Arc::new(InMemoryTenantStore::new());
        tenants.insert_unchecked("secret", Tenant::new("acme", "cus_acme"));
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inner_calls = calls.clone();
        let service = ServiceBuilder::new()
            .layer(MeterLayer::new(EdgeConfig::new(tenants)))
            .service(service_fn(move |_request: Request<Full<Bytes>>| {
                inner_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async {
                    Ok::<_, Infallible>(response(&json!({
                        "id":1,
                        "result":{
                            "resultType":"complete",
                            "tools":[],
                            "ttlMs":60000,
                            "cacheScope":"private"
                        }
                    })))
                }
            }));

        consume(
            service
                .clone()
                .oneshot(request(
                    "tools/list",
                    None,
                    &json!({"id":1,"params":{}}),
                    "secret",
                ))
                .await
                .unwrap(),
        )
        .await;
        consume(
            service
                .oneshot(request(
                    "tools/list",
                    None,
                    &json!({"id":2,"method":"resources/list","params":{}}),
                    "secret",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
