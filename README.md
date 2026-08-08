# UsageKit for MCP

Provider-neutral usage infrastructure for MCP 2026-07-28 servers.

`mcp-usage-kit` is an Apache-2.0 Tower layer for Rust MCP servers. It records
terminal delivery instead of counting HTTP requests, so multi-round-trip calls
bill once, task polling does not multiply an invoice, errors are free, and
discovery traffic remains free.

```sh
cargo add mcp-usage-kit
```

[Documentation](https://docs.rs/mcp-usage-kit) | [Landing page](https://pokitappz.github.io/mcp-usage-kit/) | [Conformance](docs/conformance.md) | [Changelog](CHANGELOG.md) | [Security policy](SECURITY.md)

## Workspace

- `mcp-usage-core` - pure protocol classification, pricing, terminal-delivery,
  task-attribution, quota, and spend-cap decisions.
- `mcp-usage-export` - synchronous usage recording, concurrent aggregation,
  idempotent retry, a log exporter, and a provider-neutral meter event exporter.
- `mcp-usage-tower` - API-key authentication, mandatory header/version guards,
  authorization-aware caching, task attribution, SSE observation, metrics, and
  the generic Tower layer.
- `mcp-usage-store` - atomic Redis, Valkey, and PostgreSQL task attribution for
  horizontally scaled servers.
- `mcp-usage-kit` - the public facade and runnable `rmcp` example.

The repository contains the embeddable crates only. It does not include a
hosted control plane or reverse proxy.

## The billing rule

The meter records usage only after it observes a terminal result:

| Exchange | Usage |
|---|---:|
| `tools/call` with `resultType: complete` | configured units |
| Interim MRTR `input_required` result | 0 |
| Task creation and progress polls | 0 |
| Completed task | originating tool's units, once per `taskId` |
| Failed/cancelled task or JSON-RPC error | 0 |
| Discovery and subscription traffic | 0 |
| Cache-served `resources/read` | configured units on delivery |

The edge refuses to trust mirrored headers from a protocol revision older than
2026-07-28. It also decodes the MCP Base64 name sentinel before looking up a
price. See [`docs/spec-2026-07-28-findings.md`](docs/spec-2026-07-28-findings.md)
for the wire-level reasoning and specification links.

## Run the example

```sh
MCP_API_KEY="$(openssl rand -base64 32)" cargo run -p mcp-usage-kit --example rmcp_server
```

The example exposes an `rmcp` Streamable HTTP service at
`http://127.0.0.1:3000/mcp`, prices its `add` tool at two units, and flushes
aggregated usage to structured logs every ten seconds.

In an application, wrap the `rmcp::StreamableHttpService` directly:

```rust,ignore
use std::sync::Arc;
use mcp_usage_kit::{
    BillingPipeline, EdgeConfig, InMemoryTenantStore, LogExporter,
    MeterLayer, Tenant,
};
use tower::Layer;

let tenants = Arc::new(InMemoryTenantStore::new());
tenants.insert("secret-api-key", Tenant::new("acme", "cus_acme"));

let billing = Arc::new(BillingPipeline::new(LogExporter::new()));
let edge = EdgeConfig::new(tenants).with_recorder(billing.clone());
let metered_rmcp_service = MeterLayer::new(edge).layer(rmcp_service);
```

Call `BillingPipeline::flush` from a background task. Export failures retain the
same aggregate quantity and provider event identifier for the next retry; they
never change an MCP response. Cancellation and panic restore the in-progress
batch through a drop guard. `LogExporter` is stateless and logging-only; use an
application-owned exporter when exported batches must be captured.

Implement `MeterEventProvider` for the billing service your application uses,
then wrap it in `MeterEventExporter`. Providers receive unresolved aggregates in
their original order and return one `MeterEventOutcome` per aggregate. Accepted
and permanently rejected events are not resubmitted; retryable and ambiguous
events keep their stable identifiers. Permanent rejections enter a bounded,
identifier-deduplicated dead letter queue for application-owned reconciliation.
Verified asynchronous rejections can enter the same queue through
`quarantine_async_rejection`. Alert on `dropped_dead_letters` because a nonzero
value means reconciliation data was discarded.

Provider implementations own authentication, transport, wire encoding,
timestamp acceptance, response classification, and asynchronous event
verification. They must use stable aggregate identifiers for idempotency and use
only static, low-cardinality codes with no secrets or customer data in outcomes
and batch-wide errors. See
[`docs/integration-contracts.md`](docs/integration-contracts.md) for the complete
contract.

## Distributed task attribution

Enable `redis` or `postgres` on `mcp-usage-kit` when durable tasks can be polled
through more than one application instance. Both stores hash tenant and task
identifiers before persistence. The edge resolves the price when a task is
created, and the stores retain only that integer price plus a fixed method
category. Tool names, prompt names, resource URIs, and extension method strings
are not persisted. Records expire and are atomically claimed so only one
instance records a completed task.

The in-memory store remains the default for single-process deployments. See
[`mcp-usage-store`](crates/mcp-usage-store/README.md) for setup and operational
requirements. Exact dependency and provider assumptions are recorded in
[`docs/integration-contracts.md`](docs/integration-contracts.md).

## Observability and conformance

`EdgeMetrics::render_prometheus` emits low-cardinality Prometheus text without a
registry dependency. The optional `opentelemetry` feature registers pull-based
observable counters with an application-owned meter. Neither path exports API
keys, tenant identifiers, customer identifiers, methods, or tool names.

Machine-readable versioned test vectors live under
[`mcp-usage-core/conformance`](crates/mcp-usage-core/conformance/v1/cases.json).
They are independent of Rust and can be reused by gateways and servers in other
languages.

## Security and operational bounds

- API keys are stored as SHA-256 lookup hashes, never plaintext.
- Requests with duplicate security metadata or both supported credential
  mechanisms are rejected before reaching the origin.
- Private cache entries are partitioned by the exact authorization context, not
  merely by tenant, and JSON-RPC IDs are rewritten on cache hits.
- Results the origin marks `cacheScope: "public"` are **not** shared across
  tenants by default. Sharing them means trusting the origin's assertion that a
  body does not depend on who asked for it, and `resources/read` is cacheable, so
  one mislabelled result would disclose resource contents across tenants. Opt in
  with `EdgeConfig::with_public_cache_sharing` when that assertion holds.
- A cache hit requires an exact match between the mirrored method and name
  headers and the JSON-RPC body.
- MRTR continuations are never cached.
- The API key is consumed by the edge and stripped before the request reaches the
  inner service. Enable `EdgeConfig::with_credential_forwarding` when the origin
  performs its own check against the same key.
- Responses generated by the edge carry `X-Content-Type-Options: nosniff` and
  cache directives that keep them out of shared downstream caches.
- Authentication failures are counted separately from malformed-header
  rejections, so credential stuffing is distinguishable in the metrics.
  `EdgeConfig::with_auth_failure_limit` bounds sustained guessing across the
  edge; only failures consume the budget, so enabling it cannot lock out callers
  holding valid keys. It returns an error for a zero-duration window. Per-address
  limits still belong in the proxy in front, because the edge has no client
  identity it can trust.
- `InMemoryTenantStore::insert` refuses obviously weak API keys, since a digest
  lookup only protects a secret that was hard to guess. `insert_unchecked` is the
  explicit escape hatch for fixtures.
- Durable-task accounting that cannot finish synchronously is parked on
  `EdgeConfig::deferred` rather than dropped. Later authenticated requests drain
  it automatically; drain it explicitly after graceful shutdown.
- W3C `traceparent` headers pass through unchanged.
- The default request body cap is 1 MiB. Oversized requests are rejected with
  `413 Payload Too Large`; configure it with `EdgeConfig::with_max_request_body`.
- The default response observation cap is 1 MiB. An oversized response still
  streams normally but fails toward not charging and not caching; configure the
  cap for servers that legitimately return larger JSON results.
- The default task-attribution store retains at most 100,000 entries and the
  idempotency store retains at most 100,000 keys. Both are process-local. A
  horizontally scaled hosted edge must provide shared implementations of the
  exposed store traits.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo bench -p mcp-usage-tower --bench classification
```

The integration suite wraps a real `rmcp` 3.1 Streamable HTTP service and
asserts that the configured tool units reach the billing exporter.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
