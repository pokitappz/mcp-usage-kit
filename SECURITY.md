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
  separately from malformed-header rejections.
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
  `EdgeConfig::deferred`. Subsequent requests drain it automatically, but drain
  it explicitly on shutdown so a departing process does not take durable-task
  charges with it. Alert on `DeferredCompletions::dropped`, which is nonzero only
  when usage was discarded because the queue was full.
- **Cross-tenant cache sharing.** `EdgeConfig::with_public_cache_sharing` is off
  by default. Turning it on trusts every origin behind the layer to use
  `cacheScope: "public"` only for results that genuinely do not depend on the
  caller.
- **Transport.** The layer is TLS-agnostic on the inbound side. Terminate TLS in
  front of it, and note that the bundled examples bind loopback only.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability. Use the
repository's [private security advisory form](https://github.com/pokitappz/mcp-usage-kit/security/advisories/new)
and include reproduction steps, affected versions, and the expected impact.

Reports are handled privately. We will coordinate disclosure and credit with the
reporter after a fix is available.
