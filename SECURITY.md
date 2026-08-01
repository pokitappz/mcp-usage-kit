# Security policy

## Supported versions

Security fixes are provided for the latest released minor version. Before the
first stable release, fixes may require upgrading to the newest 0.x release.

## Deployer responsibilities

These crates are middleware, not a service. Some controls can only be applied by
the application that embeds them.

- **Rate limiting.** The edge authenticates an API key on every request but does
  not throttle failures, so nothing here bounds a credential-stuffing attempt.
  Stack a Tower rate-limit layer ahead of `MeterLayer` and alert on
  `mcp_usage_unauthenticated_total`, which counts credential failures separately
  from malformed-header rejections.
- **API-key entropy.** Keys are compared by SHA-256 digest, which is a lookup
  hash and not a password hash. That is appropriate only for high-entropy
  secrets; generate keys from a CSPRNG rather than accepting user-chosen values.
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
