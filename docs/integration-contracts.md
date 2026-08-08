# Integration contracts

This document records the external crate and service contracts used by optional
UsageKit features. Re-check the linked upstream documentation before changing a
major version or wire format.

## Redis and Valkey

- Client: `redis` 1.5.0 with the async connection manager and Tokio Rustls.
- Connection behavior: one cloneable multiplexed connection manager is reused;
  it reconnects after transient failures.
- Timeouts: connection and command operations default to two seconds and can be
  configured with `RedisTaskStore::connect_with_timeout`.
- Writes: `SET key value NX EX seconds` preserves the first pre-priced task
  attribution and expires abandoned tasks. TTLs are validated as positive
  signed 64-bit seconds before the client parses a URL or opens a connection,
  matching Redis integer expiry arguments.
- Claims: a single `EVAL` operation reads and deletes the attribution atomically.
- Privacy: keys contain SHA-256 digests of tenant and task IDs. Values use a
  versioned 10-byte record containing only a fixed method category and the
  resolved unsigned integer price. No identifier, name, URI, or extension
  method text is persisted.
- References: [Redis `SET`](https://redis.io/docs/latest/commands/set/),
  [Redis signed integer representation](https://redis.io/docs/latest/develop/reference/protocol-spec/),
  [Redis Rust client guidance](https://redis.io/docs/latest/develop/clients/rust/json/),
  and the [`redis` connection manager API](https://docs.rs/redis/1.5.0/redis/aio/struct.ConnectionManager.html).

## PostgreSQL

- Client: SQLx 0.8.6 with Tokio and Rustls.
- Version choice: SQLx 0.9.0 requires Rust 1.94.0, so the workspace stays on
  0.8.6 to preserve its Rust 1.88 minimum.
- Queries: runtime prepared queries with bound parameters. No tenant, task, or
  configuration value is interpolated into SQL.
- Timeouts: every schema and query operation defaults to two seconds and can be
  configured with `PostgresTaskStore::with_timeout`.
- Claims: `DELETE ... RETURNING` atomically consumes one completed task
  attribution.
- Schema: [`crates/mcp-usage-store/schema/postgres.sql`](../crates/mcp-usage-store/schema/postgres.sql).
- Privacy: the schema stores hashed tenant and task IDs plus the same versioned
  10-byte attribution used by Redis. It has no method-text or name column.
- Pre-release schema change: installations created from an earlier commit must
  reconcile or expire live task records, then drop and reinstall
  `mcp_usage_task_attribution`. `PostgresTaskStore::install` rejects the old
  shape instead of silently using it.
- References: [SQLx 0.8.6](https://docs.rs/sqlx/0.8.6/sqlx/),
  [SQLx pooling](https://docs.rs/sqlx/0.8.6/sqlx/struct.Pool.html).

## OpenTelemetry

- API crate: `opentelemetry` 0.32.0 with metrics only.
- Ownership: the application supplies its configured `Meter`; UsageKit does not
  install an SDK, exporter, global provider, or network transport.
- Instruments: thirteen pull-based observable counters with fixed names and no
  identifying labels.
- Status: the upstream Rust metrics API is currently marked beta.
- References: [OpenTelemetry Rust status](https://opentelemetry.io/docs/languages/rust/),
  [`Meter` API](https://docs.rs/opentelemetry/0.32.0/opentelemetry/metrics/struct.Meter.html).

## Meter event providers

`MeterEventProvider::submit` receives a batch of unresolved `AggregatedUsage`
values in original order. It returns exactly one ordered `MeterEventOutcome` per
submitted aggregate, or one `MeterEventProviderError` when no per-event outcome
is available. A wrong outcome count is treated as a batch-wide failure and no
returned outcomes are applied.

Providers own authentication, endpoint selection, transport security, request
encoding, timestamp validation, response classification, and any asynchronous
notification receiver. They must use `AggregatedUsage::identifier` as the
provider-side idempotency identity because cancellation, panic, timeout, and
batch-wide failures can leave an accepted event ambiguous. Provider-specific
timestamp windows or asynchronous acceptance semantics belong in the provider,
not the generic exporter.

Outcome and error codes must be static, low-cardinality categories with no
credentials, customer data, event identifiers, URLs, response bodies, or other
identifying values. `Accepted` and `PermanentRejection` complete an event.
Permanent rejections enter a bounded reconciliation queue. `RetryableFailure`
leaves an event unresolved and causes the full pipeline batch to remain pending.

The exporter retains process-local partial progress and accepts only the
identical original batch until all events complete or the application calls
`abandon_retry_progress`. Retries submit only unresolved aggregates in original
order. Applications may pass a verified and correlated late provider rejection
to `quarantine_async_rejection` so synchronous and asynchronous rejection use
the same bounded, identifier-deduplicated reconciliation queue.

### Adapter shape

A provider adapter normally maps each aggregate to its own request payload and
keeps the stable `identifier` in that payload. A typical hosted meter payload
carries an event name, customer identifier, integer value, timestamp, and
idempotency identifier. The generic exporter does not construct that wire format
or apply its timestamp rules. The adapter sends the payloads,
preserves their order, and translates each response into one of:

- `Accepted` when the provider confirms the event.
- `RetryableFailure { code: "unavailable" }` when the event remains unresolved.
- `PermanentRejection { code: "invalid_event" }` when only that event is invalid.

If no per-event result exists, return a batch-wide
`MeterEventProviderError::new("unavailable")`. The complete adapter example,
including the boxed future shape, is in the
[`mcp-usage-export` README](../crates/mcp-usage-export/README.md#provider-implementation-example).
