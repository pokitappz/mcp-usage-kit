# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-09-22

Breaking. Two public signatures changed, both to fix behaviour that was wrong
or expensive on every request. A `0.3` caller needs two edits, listed first.

### Changed

- **Breaking.** `TenantStore::authenticate` returns `Option<Arc<Tenant>>` rather
  than `Option<Tenant>`. It runs once per request and a `PriceBook` is two
  `BTreeMap`s, so returning a value deep-copied every priced tool name on every
  call. An implementation only needs to wrap its return in `Arc::new`; a caller
  reading fields through the `Arc` needs no change at all.
- **Breaking.** `ResponsePeek::with_legacy_delivery` takes the originating
  `&Method`. It used to promote any result on a legacy revision to a delivery,
  which billed an idle legacy client for its own lifecycle traffic forever. It
  now promotes only methods that deliver priced work.
- `MetricsSnapshot` gained `free_by_reason` and `unrecognized_by_reason`.
  Exhaustive struct literals break; it still derives `Default`, so
  `..Default::default()` does not.
- Cut the per-request cost of the metered path. Measured end to end through
  `MeterLayer` against an in-memory origin by the new `hot_path` bench, so the
  figures are meter overhead rather than origin work; median of three runs:

  | case | 0.3.1 | 0.4.0 | |
  |---|---|---|---|
  | `tools/call`, 2 KB body, 1 priced tool | 11.2 us | 9.4 us | -16% |
  | `tools/call`, 2 KB body, 64 priced tools | 14.8 us | 9.1 us | -38% |
  | `tools/call`, 128 KB body, 64 priced tools | 122 us | 108 us | -11% |
  | `tools/call` over SSE, 64 priced tools | 16.0 us | 9.6 us | -40% |
  | `tools/list` cache hit, 64-tool listing | 348 us | 196 us | -44% |

  Non-cacheable methods no longer parse the whole body into a `serde_json::Value`
  to read four fields; the SSE reader no longer rewrites the captured body twice
  to normalize line endings; a cache hit no longer deep-copies the stored
  response under the cache mutex to overwrite one field, and `insert` no longer
  sweeps the whole cache on every call.
- `BillingPipeline` alternates between the retry queue and fresh usage after a
  failure instead of always serving retries first, and reports `retry_buckets`
  separately from `pending_buckets`.
- The edge's quota gate is `AdmissionLayer`, not `QuotaLayer`: it decides
  admission on two grounds now rather than one.

### Added

- `mcp-usage-edge`, a metering sidecar that fronts an MCP server written in any
  language. Reverse proxy plus `MeterLayer`, TOML configuration, a control-plane
  tenant cache that fails closed past a staleness budget, and a distroless
  image. **This crate has never been published; see the "First release" section
  of `RELEASE.md` before tagging.**
- A Python binding over `mcp-usage-core`, published to PyPI as `mcp-usage-kit`,
  proven against the same `conformance/v1/cases.json` vectors the Rust
  reference test runs and exercised on its declared `abi3-py39` floor.
- Inbound MPP support at the edge: with an `[mpp]` section an over-quota call
  is priced rather than refused, answering 402 with a `WWW-Authenticate: Payment`
  challenge per `draft-ryan-httpauth-payment-01`. The credential rides
  `Payment-Authorization`, because `Authorization` carries the tenant's own key
  to the upstream.
- `FreeReason::ALL`, `::as_str` and `::index`, which define the eleven wire
  names once so metrics, the binding and the conformance vectors cannot drift
  into three spellings.
- `UnaccountedReason`, naming the four unrelated causes that previously shared
  `unrecognized_responses`.
- Two Prometheus families, `mcp_usage_free_deliveries_by_reason_total` and
  `mcp_usage_unrecognized_responses_by_reason_total`, both labelled `reason`
  from a closed enum and emitted for every reason on every scrape so a `rate()`
  resolves before that reason first fires. `reason` is the only label the edge
  emits.
- `[task_store]` in the sidecar, wiring `RedisTaskStore` behind a `redis`
  feature. A `[task_store]` section in a build without the feature is refused at
  startup rather than ignored.

### Fixed

- A legacy client's lifecycle traffic was billed as delivered work, forever, for
  as long as it stayed connected.
- A completed task whose verdict came back free had its attribution destroyed,
  so every later poll reported a missing attribution and the charge was lost
  permanently.
- One batch the billing provider refused blocked every other customer's usage
  from ever being exported, and the buffer is in memory, so the next deploy took
  all of it.
- `flusher.abort()` only scheduled cancellation, so a SIGTERM during an export
  could exit with the whole buffer unflushed. The handle is awaited.
- Permanently rejected aggregates were quarantined into a queue nothing read and
  silently evicted when it filled. The flush loop drains them and raises an
  error when retention has discarded anything.
- The sidecar used the process-local task store, so a task created on one
  instance and completed on another billed nothing: roughly `1 - 1/N` of
  durable-task revenue on an N-instance deployment.
- A `PostgresTaskStore` insert used `ON CONFLICT DO NOTHING`, which ignores
  `expires_at`. A task id reused after its attribution expired reported success,
  wrote nothing, and was unbillable from then on.
- An MPP credential bought for a cheap call could redeem an expensive one; two
  legitimate payments in the same second could collide and destroy one; the
  request body was buffered before its size limit was checked; and the
  spent-proof set was cleared wholesale when full, which made an already-spent
  credential replayable. A failed credential no longer hard-refuses a tenant who
  is inside quota, `Payment-Authorization` no longer reaches the upstream, and a
  pricing failure no longer issues a zero-amount challenge.
- `LimitReason::ArithmeticOverflow` reported two different wire names from the
  binding and the sidecar. Both say `usage_unrepresentable`.
- `Meter.decide` and `Meter.price` decode the `Mcp-Name` sentinel form
  themselves, so passing the raw header no longer silently charges every
  non-ASCII-named tool the default price.

### Documentation

- The durable-task store TTL is a deadline on the whole task, not on the polling
  interval, and a task that outlives it loses its charge silently. Documented on
  the crate root and both constructors, with the metric to watch and why a
  per-task TTL needs a `TaskAttributionStore` change rather than a backend one.
- `terminal_response` records why an undeclared or unrecognized `Content-Type`
  is still not read as MCP: the meter will not infer a charge from a body whose
  sender did not say it was MCP. What was wrong was the silence, not the
  refusal.

## [0.3.1] - 2026-09-08

### Changed

- Updated dependencies to their current semver-compatible versions: `hyper`
  1.11.1, `http-body-util` 0.1.5, `redis` 1.6.0, `rmcp` 3.2.0, `thiserror`
  2.0.20, and `uuid` 1.26.0. No public API changed, and the minimum supported
  Rust version stays at 1.88.
- Moved the locked `chacha20` off 0.10.1, which upstream has since yanked, onto
  0.10.2. It is reached only through `rand` and `rmcp`, both development
  dependencies, so no published crate was affected.

### Fixed

- Removed references to a specification-findings document that no longer exists,
  including a broken relative link in the README. The reasoning each reference
  pointed at is now stated where it is needed.

## [0.3.0] - 2026-08-08

### Changed

- Replaced the optional Stripe HTTP exporter with the always-available,
  dependency-free `MeterEventProvider` and `MeterEventExporter` abstraction.
  Providers return one ordered outcome per submitted aggregate. The exporter
  retains partial retry progress, submits only unresolved events, and provides a
  bounded reconciliation queue for synchronous and asynchronous rejections.

### Removed

- Removed the `stripe` Cargo features, `reqwest` dependency, `StripeExporter`,
  `StripeDeadLetter`, and `StripeDeadLetterReason`. Provider implementations now
  own authentication, transport, timestamp policy, response classification, and
  wire encoding.

## [0.2.0] - 2026-08-01

### Added

- Renamed the facade and component crates under the `mcp-usage` namespace.
- Per-crate package metadata, license files, and READMEs.
- Static project site with an interactive billing example.
- Continuous integration, GitHub Pages deployment, and trusted publishing
  workflows.
- Terminal-delivery billing semantics for MCP 2026-07-28.
- MRTR and durable task attribution without request-count overbilling.
- Tower authentication, caching, body bounds, metrics, and usage recording.
- Buffered log and optional Stripe Billing Meter Events exporters.
- Redis, Valkey, and PostgreSQL durable-task attribution with hashed keys,
  expiry, first-writer preservation, and atomic completion claims.
- Low-cardinality Prometheus output and optional OpenTelemetry observable
  counters.
- Provider-neutral function and composite exporters.
- Versioned machine-readable usage-accounting conformance vectors.
- Compiling Axum, Hyper, and `rmcp` integration examples.
- `EdgeConfig::with_public_cache_sharing` and
  `EdgeConfig::with_credential_forwarding`.
- An `unauthenticated` counter, exposed as `mcp_usage_unauthenticated_total` and
  the `mcp.usage.unauthenticated` OpenTelemetry instrument, separating credential
  failures from malformed-header rejections.
- `EdgeConfig::deferred` exposes the queue of terminal accounting that could not
  finish synchronously, with `drain` and `drain_some`. Every subsequent
  authenticated request runs a bounded number automatically, so an application
  that ignores it still converges; draining explicitly is timelier, and draining
  after graceful shutdown stops a departing process from taking durable-task
  charges with it. Tunable through `EdgeConfig::with_deferred_capacity` and
  `EdgeConfig::with_deferred_drain_per_request`. No runtime dependency is
  introduced: nothing is spawned.
- A `deferred` counter, exposed as `mcp_usage_deferred_completions_total` and the
  `mcp.usage.deferred_completions` OpenTelemetry instrument, plus
  `DeferredCompletions::dropped` reporting accounting discarded because the queue
  was full.
- Property tests covering every parser that reads untrusted input: the SSE
  terminal-response reader, the JSON request and response peeks, the base64
  `Mcp-Name` sentinel decoder, the protocol-version guard, and the request
  inspector. They run on stable in ordinary CI. Coverage-guided fuzz targets for
  the same surface live in `fuzz/`, outside the workspace because libFuzzer
  requires nightly, and are built and smoke-run by CI so they cannot rot.
- Integration tests exercising the Redis and `PostgreSQL` task stores against
  real backends, covering the invariants billing correctness rests on: a task
  origin is immutable once captured, a completed task can be claimed exactly
  once under concurrent contention, records are isolated per tenant, and
  abandoned records expire. Each backend is skipped unless its URL is present in
  the environment, so `cargo test` stays green without a database; CI sets
  `MCP_USAGE_REQUIRE_BACKENDS=1` so a broken service container fails instead of
  silently skipping.
- `StripeExporter` retains aggregates strictly older than Stripe's 35-day Meter
  Event window in a bounded dead letter queue. `StripeDeadLetter`,
  `StripeDeadLetterReason`, `with_dead_letter_capacity`, `dead_letter_count`,
  `dropped_dead_letters`, and `take_dead_letters` support explicit
  reconciliation without rewriting timestamps.
- Stripe reconciliation also quarantines timestamps more than five minutes in
  the future and individual events synchronously rejected as permanently
  invalid, without blocking later valid events in the batch.
- `StripeExporter::quarantine_async_rejection` lets an application retain an
  original aggregate after it verifies and correlates Stripe's asynchronous
  Meter Event failure notification.
- Stripe transient retries retain process-local partial-batch progress, require
  the identical batch, and skip events whose successful responses were already
  confirmed.
- `StripeExporter::retry_in_progress` and `StripeExporter::abandon_retry_progress`
  report and release an incomplete batch. Abandoning one returns the identifiers
  Stripe never confirmed, for application-owned reconciliation.

### Changed

- `LogExporter` is now a stateless logging sink. Tests and applications that
  need captured batches should provide their own `BatchExporter`.
- `EdgeConfig::with_auth_failure_limit` returns `Result` and rejects a
  zero-duration window with `EdgeConfigError::InvalidAuthFailureWindow`.
- `UsageBuffer`, `BillingPipeline`, and exporter Debug output now exposes counts
  and configuration bounds only, without buffered identifiers or payloads.
- `TaskAttributionStore` now persists `TaskAttribution` instead of `Call`.
  `TaskAttribution::from_call` resolves the price when a task is created and
  discards identifying names and extension method text.

### Fixed

- Durable-task accounting is no longer lost when a task store performs real I/O.
  Terminal accounting runs as the response body is released, which happens in
  `Drop`, where nothing may await. The in-process store never yields so it always
  finished there, but a Redis or `PostgreSQL` store pends on its first poll and
  the work was abandoned. Measured against Redis over real HTTP, a complete
  durable-task lifecycle recorded `billed=0` with nothing written to the store at
  all: the attribution `insert` never landed, so the completing poll had nothing
  to price. Every durable-task charge was lost, in exactly the horizontally
  scaled deployment those stores exist for. Unfinished accounting is now parked
  on `DeferredCompletions` and driven from a context that can await.
- Terminal accounting now also runs when the response body is dropped, not only
  when it is polled to end-of-stream. A transport is not obliged to make that
  final poll, and hyper stops as soon as the bytes declared by `Content-Length`
  have been written, which is the ordinary case for a fixed-length JSON result.
  As a result billing, cache insertion, and durable-task attribution silently did
  nothing behind Axum, Hyper, and `rmcp`: three billable `tools/call` requests
  recorded `classified=3` but `billed=0`. The completion future is polled once
  with a no-op waker, which covers synchronous recorders and the default
  in-memory task store; a task store performing real I/O that cannot complete
  synchronously increments `record_failures` instead of being dropped silently.
- `PostgresTaskStore::install` is now safe to call concurrently. `CREATE TABLE
  IF NOT EXISTS` is not atomic in `PostgreSQL`: racing sessions each consult the
  catalog, all conclude the table is absent, and every loser fails with a
  duplicate key violation on `pg_type_typname_nsp_index`. Every instance of a
  horizontally scaled application calls this on boot, and they boot together, so
  all but one would fail to start. A transaction-scoped advisory lock now
  serializes the check.
- Cancelling or unwinding an in-progress `BillingPipeline::flush` restores the
  exact batch, including quantities and stable identifiers, before another
  flush can begin.
- Cancelling `DeferredCompletions::drain` requeues the in-flight future instead
  of losing it. Automatic draining now starts only after authentication, so
  malformed and unauthenticated traffic cannot trigger backend work.
- A claimed durable-task origin is restored when the usage recorder rejects the
  charge, allowing a later terminal poll to retry it.
- Repeated in-memory task insert and claim cycles compact stale insertion-order
  records and keep bookkeeping bounded near twice the configured live capacity.
- SSE parsing normalizes CRLF, LF, and bare CR event-stream line endings.
- Redis and Valkey TTLs larger than a signed 64-bit expiry argument are rejected
  before any connection attempt.
- The final example drain runs only after graceful server shutdown has released
  all response bodies, and the example command now supplies a key accepted by
  the entropy validator.
- A partially-exported Stripe batch no longer wedges the exporter permanently.
  Requiring the identical batch on retry is what keeps confirmed events from
  being resubmitted, but if that batch's owner went away without retrying it,
  every later batch was refused and billing stopped with no way to recover.
- `Authorization: bearer` is accepted. RFC 7235 makes the scheme name
  case-insensitive; matching it exactly turned a conforming request into a `401`
  indistinguishable from a wrong key.
- A Stripe endpoint whose path is wrong fails loudly instead of discarding every
  event. `404` and `402` describe the request or the account rather than the
  event, so treating them as permanent per-event rejections quarantined all
  usage and reported a successful export.
- A cache lookup no longer sweeps the whole cache. Expiry is checked on the keys
  actually read, and an expired entry found there is dropped, so read cost no
  longer grows with `EdgeConfig::with_cache` capacity while holding the lock.
- `release.yml` refuses to run for a crate that is not yet on crates.io. Trusted
  publishing cannot work for a crate's first version, so the run previously
  failed at authentication after most of its work, with a partial publish
  possible. See the first-release procedure in `RELEASE.md`.

### Security

- Results the origin marks `cacheScope: "public"` are no longer shared across
  authorization contexts by default. Honoring that hint places one entry in a
  bucket every tenant reads, so an origin that mislabels a tenant-specific
  result would disclose it across tenants; `resources/read` is cacheable, so the
  exposure is resource contents rather than discovery listings alone. Re-enable
  with `EdgeConfig::with_public_cache_sharing(true)`.
- The API key is stripped from the request before it reaches the inner service.
  The edge has already consumed the credential, and forwarding it hands a secret
  to a service that was not issued it. Re-enable with
  `EdgeConfig::with_credential_forwarding(true)` when the origin performs its own
  check against the same key.
- `InMemoryTenantStore::insert` refuses obviously weak API keys, and returns
  `Result`. Keys are compared by SHA-256 digest, which is a lookup hash rather
  than a password hash, so it only protects a secret that was hard to guess to
  begin with. `validate_api_key_strength` is public for validating keys wherever
  they are loaded, and `insert_unchecked` is the explicit escape hatch for
  fixtures and for keys validated elsewhere. The check is a guardrail against
  mistakes, not a strength certificate.
- `EdgeConfig::with_auth_failure_limit` bounds sustained credential guessing
  across the edge. Disabled by default. Only failures consume the budget, so
  enabling it cannot lock out callers holding valid keys: exhausting it turns a
  wrong key's `401` into a `429` and nothing else. This is not per-client
  limiting, which needs a client identity the edge cannot trust.
- Redis, Valkey, and PostgreSQL task-attribution values no longer contain raw
  tool names, prompt names, resource URIs, or extension method strings. Their
  versioned binary record contains only a fixed method category and resolved
  integer units.
- Responses generated by the edge carry `X-Content-Type-Options: nosniff`, plus
  `Cache-Control: no-store` on errors and `Cache-Control: private` on cache hits,
  so a shared downstream cache cannot reuse a tenant-specific body.
- `StripeExporter` refuses HTTP redirects, so the endpoint allowlist is now
  terminal. Previously a 3xx could carry the meter event, including the customer
  identifier, to a host that was never checked.
- `StripeExporter::with_endpoint` validates the endpoint immediately and returns
  `Result`, instead of accepting a disallowed value and failing at the first
  flush.
- The workspace links one rustls crypto provider instead of two. `reqwest` and
  `redis` both pin aws-lc-rs with no way to choose otherwise, so `sqlx` moved
  from `tls-rustls-ring` to `tls-rustls-aws-lc-rs`, which is the only direction
  that removes a backend. `ring` is no longer compiled.
- Added `cargo-deny` license, source, and ban gates; pinned every GitHub Action
  to a commit SHA; and scheduled the advisory audit to run daily rather than only
  on push.
- A checked-in administrator script configures every CI job as required before
  `main` can advance, makes release tags immutable, and review-gates the release
  environment. The publish workflow independently verifies that a version tag
  points to a commit already on `main`.
- `scripts/check-release-version.sh` additionally verifies that every
  workspace-internal dependency requirement matches the workspace version. Those
  requirements resolve against the registry rather than the path, so one left at
  the previous version is invisible to `cargo package` and to every other CI job,
  then fails partway through the dependency-ordered publish with the earlier
  crates already released and unpublishable again. The internal requirements are
  now declared once in `[workspace.dependencies]`.
- Both static pages declare restrictive Content Security Policies. The 404 page
  no longer embeds CSS, and site checks reject inline executable content.
- CI backend images are pinned by digest as well as version. First-release
  archive inspection uses `cargo package --workspace --locked --no-verify`,
  while dependency-ordered publishing retains Cargo's verification step.
