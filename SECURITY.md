# Security policy

## Supported versions

Security fixes are provided for the latest released minor version. Before the
first stable release, fixes may require upgrading to the newest 0.x release.

## Deployer responsibilities

These crates are middleware, not a service. Some controls can only be applied by
the application that embeds them.

- **Rate limiting.** `EdgeConfig::with_auth_failure_limit` bounds sustained
  credential guessing across the edge. It is disabled by default, because the
  useful ceiling depends on how many clients you serve. Only failures consume the
  budget, so enabling it cannot lock out callers holding valid keys. It is not
  per-client limiting: the edge has no client identity it can trust, since a
  source address belongs to the transport and forwarding headers are attacker
  controlled, so per-address limits belong in the proxy in front of it. Alert on
  `mcp_usage_throttled_total` and `mcp_usage_unauthenticated_total`, both counted
  separately from malformed-header rejections. The builder returns
  `InvalidAuthFailureWindow` for a zero-duration window.
- **API-key entropy.** Keys are compared by SHA-256 digest, which is a lookup
  hash and not a password hash, so it protects only a secret that was hard to
  guess to begin with. `InMemoryTenantStore::insert` refuses obviously weak keys;
  `insert_unchecked` bypasses that for fixtures and for keys validated at the
  boundary where they are issued. The check is a guardrail against mistakes, not
  a strength certificate: it cannot see structure, so a long repeating pattern
  drawn from a wide alphabet passes. Generate keys from a CSPRNG, for example
  `openssl rand -base64 32`.
- **Durable-task accounting.** With a Redis or `PostgreSQL` task store, terminal
  accounting that cannot finish synchronously is parked on
  `EdgeConfig::deferred`. Authenticated requests drain it automatically;
  malformed and unauthenticated traffic cannot trigger backend work. A
  cancelled drain requeues its in-flight completion. Drain explicitly after the
  server has completed graceful shutdown so a departing process does not take
  durable-task charges with it. Alert on `DeferredCompletions::dropped`, which
  is nonzero only when usage was discarded because the queue was full.
- **Billing durability.** Cancellation and panic during `BillingPipeline::flush`
  restore the exact aggregate quantities, timestamps, and identifiers for retry.
  The buffer remains process-local and does not survive process loss. Strong
  crash durability requires an application-owned durable recorder or outbox.
- **Provider reconciliation.** `MeterEventExporter` retains synchronous
  permanent rejections and verified asynchronous rejections in a bounded,
  identifier-deduplicated dead letter queue. Applications must authenticate and
  verify asynchronous provider notifications, correlate them with source usage,
  and pass only the original aggregate to `quarantine_async_rejection`. Alert on
  `dropped_dead_letters` because a nonzero value means reconciliation data was
  discarded.
- **Provider retries and diagnostics.** Partial progress is process-local and
  requires the identical original batch on retry. Confirmed events are skipped;
  unresolved events keep their stable identifiers. Provider implementations
  must make those identifiers idempotent across ambiguous retries. Outcome and
  batch-error codes must use a bounded set of static categories and must not
  include credentials, customer data, or other identifying values. Applications
  that require crash-durable reconciliation must retain their own source audit
  trail.
- **Cross-tenant cache sharing.** `EdgeConfig::with_public_cache_sharing` is off
  by default. Turning it on trusts every origin behind the layer to use
  `cacheScope: "public"` only for results that genuinely do not depend on the
  caller.
- **Transport.** The layer is TLS-agnostic on the inbound side. Terminate TLS in
  front of it, and note that the bundled examples bind loopback only.
- **Identifier pseudonymization.** Durable stores use deterministic SHA-256 keys.
  This keeps plaintext identifiers out of backend key names but does not prevent
  dictionary recovery of low-entropy values. A keyed-hash migration requires a
  coordinated key-format rollout and is not part of the current API.
- **Repository and release controls.** The required administrative baseline is
  encoded in `scripts/configure-github-controls.sh`: updates to `main` require a
  code-owner-approved pull request, resolution of review threads, and every CI
  job from the GitHub Actions app. Version tags are immutable, and the release
  environment requires review and accepts only `v*` tags. Independently of
  repository settings, publishing refuses a tagged commit that is not
  reachable from the current `origin/main`.
- **Task-attribution payloads.** The edge resolves named pricing when a durable
  task is created. Stores retain only the unsigned integer price and a fixed
  method category. Tool names, prompt names, resource URIs, and extension method
  strings are never written to Redis, Valkey, or PostgreSQL.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability. Use the
repository's [private security advisory form](https://github.com/pokitappz/mcp-usage-kit/security/advisories/new)
and include reproduction steps, affected versions, and the expected impact.

Reports are handled privately. We will coordinate disclosure and credit with the
reporter after a fix is available.
