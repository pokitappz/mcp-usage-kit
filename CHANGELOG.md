# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `LogExporter` is now a stateless logging sink. Tests and applications that
  need captured batches should provide their own `BatchExporter`.
- `EdgeConfig::with_auth_failure_limit` returns `Result` and rejects a
  zero-duration window with `EdgeConfigError::InvalidAuthFailureWindow`.
- `UsageBuffer`, `BillingPipeline`, and exporter Debug output now exposes counts
  and configuration bounds only, without buffered identifiers or payloads.

### Added

- `StripeExporter` retains aggregates strictly older than Stripe's 35-day Meter
  Event window in a bounded dead letter queue. `StripeDeadLetter`,
  `StripeDeadLetterReason`, `with_dead_letter_capacity`, `dead_letter_count`,
  `dropped_dead_letters`, and `take_dead_letters` support explicit
  reconciliation without rewriting timestamps.

### Fixed

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

### Security

- Both static pages declare restrictive Content Security Policies. The 404 page
  no longer embeds CSS, and site checks reject inline executable content.
- CI backend images are pinned by digest as well as version. First-release
  archive inspection uses `cargo package --workspace --locked --no-verify`,
  while dependency-ordered publishing retains Cargo's verification step.

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

### Security

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
- The workspace links one rustls crypto provider instead of two. `reqwest` and
  `redis` both pin aws-lc-rs with no way to choose otherwise, so `sqlx` moved
  from `tls-rustls-ring` to `tls-rustls-aws-lc-rs`, which is the only direction
  that removes a backend. `ring` is no longer compiled.

### Added

- Property tests covering every parser that reads untrusted input: the SSE
  terminal-response reader, the JSON request and response peeks, the base64
  `Mcp-Name` sentinel decoder, the protocol-version guard, and the request
  inspector. They run on stable in ordinary CI. Coverage-guided fuzz targets for
  the same surface live in `fuzz/`, outside the workspace because libFuzzer
  requires nightly, and are built and smoke-run by CI so they cannot rot.
- `EdgeConfig::deferred` exposes the queue of terminal accounting that could not
  finish synchronously, with `drain` and `drain_some`. Every subsequent
  authenticated request runs a bounded number automatically, so an application
  that ignores it still converges; draining explicitly is timelier, and draining
  after graceful shutdown stops a departing process from taking durable-task
  charges with it. Tunable through
  `EdgeConfig::with_deferred_capacity` and
  `EdgeConfig::with_deferred_drain_per_request`. No runtime dependency is
  introduced: nothing is spawned.
- A `deferred` counter, exposed as `mcp_usage_deferred_completions_total` and the
  `mcp.usage.deferred_completions` OpenTelemetry instrument, plus
  `DeferredCompletions::dropped` reporting accounting discarded because the queue
  was full.

- Integration tests exercising the Redis and `PostgreSQL` task stores against
  real backends, covering the invariants billing correctness rests on: a task
  origin is immutable once captured, a completed task can be claimed exactly
  once under concurrent contention, records are isolated per tenant, and
  abandoned records expire. Each backend is skipped unless its URL is present in
  the environment, so `cargo test` stays green without a database; CI sets
  `MCP_USAGE_REQUIRE_BACKENDS=1` so a broken service container fails instead of
  silently skipping.

### Fixed

- `PostgresTaskStore::install` is now safe to call concurrently. `CREATE TABLE
  IF NOT EXISTS` is not atomic in `PostgreSQL`: racing sessions each consult the
  catalog, all conclude the table is absent, and every loser fails with a
  duplicate key violation on `pg_type_typname_nsp_index`. Every instance of a
  horizontally scaled application calls this on boot, and they boot together, so
  all but one would fail to start. A transaction-scoped advisory lock now
  serializes the check.

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
- `StripeExporter` refuses HTTP redirects, so the endpoint allowlist is now
  terminal. Previously a 3xx could carry the meter event, including the customer
  identifier, to a host that was never checked.
- `StripeExporter::with_endpoint` validates the endpoint immediately and returns
  `Result`, instead of accepting a disallowed value and failing at the first
  flush.
- Responses generated by the edge carry `X-Content-Type-Options: nosniff`, plus
  `Cache-Control: no-store` on errors and `Cache-Control: private` on cache hits,
  so a shared downstream cache cannot reuse a tenant-specific body.
- Added `cargo-deny` license, source, and ban gates; pinned every GitHub Action
  to a commit SHA; and scheduled the advisory audit to run daily rather than only
  on push.

### Added

- `EdgeConfig::with_public_cache_sharing` and
  `EdgeConfig::with_credential_forwarding`.
- An `unauthenticated` counter, exposed as `mcp_usage_unauthenticated_total` and
  the `mcp.usage.unauthenticated` OpenTelemetry instrument, separating credential
  failures from malformed-header rejections.

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
