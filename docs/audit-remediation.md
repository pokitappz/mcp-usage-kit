# Audit remediation

The September 2026 remediation preserves provider wire formats and task-storage schemas.

- Export retries retain immutable batches. `BatchExporter::retry_policy()` defaults to `Interleave`; `MeterEventExporter` uses `CompleteBatch`. Composite exporters and the sidecar propagate the strictest policy. The pipeline completes that batch before sending new usage, preserving provider retry progress and identifiers.
- Legacy named calls select only the field their method executes: `resources/read` uses `params.uri`; tools and prompts use `params.name`. Conflicting fields cannot lower the price.
- Admission and metering share protocol-header validation, credential extraction, and body classification. Authentication and classification precede payment verification. Legacy payment challenges use the body identity. Transport and control messages do not require payment for a new call.
- Periodic exports first drain the current deferred-accounting backlog. SIGTERM stops acceptance, gracefully closes tracked connections after active responses finish, drains accounting, and retries all remaining batches within the shutdown budget. Exhausting that budget logs outstanding work and exits unsuccessfully.
- Control-plane and facilitator deadlines cover both response headers and bounded body collection, including chunked responses.
- Authenticated legacy GET, DELETE, notifications, and JSON-RPC responses are forwarded without creating a new billable call. Session and reconnection headers survive the proxy hop.

Optional TOML settings and defaults:

```toml
[edge]
shutdown_timeout_seconds = 10

[control_plane]
max_response_bytes = 16777216 # 16 MiB

[mpp.facilitator]
max_response_bytes = 65536 # 64 KiB
```

Each HTTP client's existing `timeout_seconds` setting now covers the whole response. Applications constructing clients directly can use `with_max_response_bytes`.

Transport behavior was checked against the [MCP 2025-11-25 transport specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), including client POST responses, GET SSE reconnection with `Last-Event-ID`, and DELETE session termination. The control-plane and facilitator HTTP schemas are this repository's own contracts; no third-party payment protocol or SDK was changed.

Regressions cover the actual pipeline/exporter composition during an outage, conflicting legacy target fields, spoofed payment headers and rejection before facilitator access, legacy transport through a proxy, stalled and oversized response bodies followed by recovery, and the actual sidecar binary receiving SIGTERM with active requests and pending/retry usage. A delayed RESP test server exercises the binary's production Redis task store during idle periods and shutdown.

Usage buffering remains in memory. Crash-durable billing and distributed payment replay protection are separate architectural work.

## Verification

- 333 workspace Rust tests passed, with required Redis/PostgreSQL integration enabled (13 backend tests).
- 31 Python tests passed against a freshly built release wheel.
- The final edge suite passed after the lifecycle refactor and SSE assertions.
- Formatting, strict Clippy, warning-free documentation, Rust 1.88 compatibility, package archives, release metadata guards, static-site checks, and the strict overbilling measurement passed.
- Workspace, Python, and fuzz dependency audits passed. Cargo-deny passed with existing duplicate-version warnings.
- All three fuzz targets built and completed 20-second smoke runs without a failure. Building synchronized the stale fuzz lockfile's local core version and base64 dependency with the current workspace.
