# Security audit

**Date:** 2026-08-01
**Scope:** the full `mcp-usage-kit` workspace (`mcp-usage-core`, `mcp-usage-tower`,
`mcp-usage-store`, `mcp-usage-export`, `mcp-usage-kit`) and its complete
dependency tree at the `Cargo.lock` pinned in this commit.

This audit was run before first publication to crates.io. It found **no
known-vulnerable dependencies and no exploitable defects**. Every finding below
is a hardening change: closing the distance between what the code guarantees and
what its documentation claimed, and adding supply-chain gates that advisory
scanning alone does not provide.

---

## Method

- **Dependencies.** All 307 packages in `Cargo.lock` were extracted and queried
  as a batch against [OSV.dev](https://osv.dev), which aggregates RustSec and the
  GitHub Advisory Database. CI additionally runs `cargo audit` against RustSec
  directly. License and source provenance were enumerated from `cargo metadata`
  with `--all-features`, so optional dependencies (`sqlx`, `redis`, `reqwest`)
  were included rather than skipped.
- **Code.** Manual review of every path that touches untrusted input: HTTP header
  classification, request and response body deserialization, the SSE terminal
  response parser, the `Mcp-Name` base64 sentinel decoder, cache key derivation
  and partitioning, credential extraction and comparison, SQL and Redis command
  construction, and every logging, metrics, and `Debug` site that could carry a
  secret or an identifier.

Reproduce the dependency scan with:

```sh
python3 - <<'PY' > /tmp/osv.json
import re, json
txt = open("Cargo.lock").read()
q = []
for block in txt.split("[[package]]")[1:]:
    n = re.search(r'^name = "(.+?)"', block, re.M)
    v = re.search(r'^version = "(.+?)"', block, re.M)
    if n and v:
        q.append({"package": {"name": n.group(1), "ecosystem": "crates.io"},
                  "version": v.group(1)})
print(json.dumps({"queries": q}))
PY
curl -s -X POST -H 'Content-Type: application/json' \
  --data @/tmp/osv.json https://api.osv.dev/v1/querybatch
```

---

## Dependency result

**307 packages, zero advisories.** No package in the tree has a known
vulnerability, and none is yanked.

Provenance and licensing are equally clean:

- All 302 third-party packages resolve from `https://github.com/rust-lang/crates.io-index`.
  No git dependencies, no vendored sources, no alternate registries.
- Every package declares a license. All are permissive; there is no copyleft
  obligation anywhere in the tree. The complete required set is `Apache-2.0`,
  `MIT`, `ISC`, `BSD-3-Clause`, `Unicode-3.0`, `CDLA-Permissive-2.0`, `Zlib`, and
  `BSL-1.0`, now enforced in `deny.toml`.

Two observations that are not defects but are worth tracking:

- Both `ring 0.17.14` and `aws-lc-rs 1.17.3` / `aws-lc-sys 0.43.0` are linked,
  because different dependencies select different rustls providers. That is two
  C and assembly crypto backends compiled from source. Consolidating on one
  provider would shrink the native-code surface.
- The resolved graph carries 14 duplicated crates it does not control:
  `getrandom`, `hashbrown`, `rand_core`, `redox_syscall`, `syn`, and the
  `windows-sys` / `windows-targets` family of target shims. `deny.toml` reports
  these as warnings rather than failing the build, so the check stays enabled
  while the duplication is resolved upstream.

---

## Findings

| ID | Finding | Severity | Status |
|----|---------|----------|--------|
| H1 | `cacheScope: "public"` results were shared across tenants with no operator control | Medium (conditional on origin behavior) | Fixed |
| H2 | Stripe endpoint allowlist was not terminal across HTTP redirects | Low | Fixed |
| H3 | The consumed API key was forwarded verbatim to the inner service | Low (deployment dependent) | Fixed |
| H4 | Edge-generated responses set no `nosniff` or cache directives | Informational | Fixed |
| H5 | Credential failures were indistinguishable from header failures in metrics | Informational | Fixed |
| H6 | No license, source, or ban gates; Actions floated on mutable tags | Low (supply chain) | Fixed |

### H1 - Public cache scope crossed the tenant boundary

`crates/mcp-usage-tower/src/cache.rs`

When an origin returned `cacheScope: "public"`, the entry was keyed with no
tenant component and served to every authenticated caller. The behavior was
spec-conformant, and the `"private"` rule was implemented correctly, but
`"public"` was trusted absolutely: a single mislabelled result at any origin
behind the layer became a cross-tenant disclosure at the edge. Because
`resources/read` is cacheable, the exposure was resource contents, not only
discovery listings.

Sharing is now opt-in through `EdgeConfig::with_public_cache_sharing`, default
off, so a public result is stored exactly like a private one unless the operator
asserts otherwise. Declining to reuse a response is always permitted, so the
conservative default remains conformant. Invalidation follows where an entry
actually landed rather than what the origin declared, so a demoted public result
still evicts the shared representation it was meant to supersede.

### H2 - Stripe allowlist was not terminal across redirects

`crates/mcp-usage-export/src/stripe.rs`

`endpoint_is_allowed` validated only the initial URL while the client followed up
to ten redirects, so a 3xx moved the request to an unchecked host. `reqwest`
strips `Authorization` across hosts, so the secret key was not exposed, but the
form body carrying `payload[stripe_customer_id]` and usage counts was. The
documented guarantee was therefore stronger than the implementation.

The client now refuses redirects, making the allowlist the last word. Separately,
`with_endpoint` validates immediately and returns `Result` instead of accepting a
disallowed value and failing only at the first flush, hours later. The
export-time check is retained as defense in depth.

### H3 - Consumed credential reached the inner service

`crates/mcp-usage-tower/src/layer.rs`

The proxied request reused the original header map, so the API key the edge had
already authenticated also reached the origin. Harmless in the documented
embedded model, where the origin runs in-process, and an exposure the moment the
layer fronts an origin in another trust domain. The credential is now stripped
before proxying, with `EdgeConfig::with_credential_forwarding` to re-enable it
for origins that perform their own check.

### H4 - No sniffing or caching directives on generated responses

`crates/mcp-usage-tower/src/layer.rs`

Rejection messages echo the offending header value back to the caller. The value
is correctly JSON-escaped and served as `application/json`, so this was **not**
exploitable in any current browser, but content-type sniffing was left open. More
materially, neither generated response told a downstream shared cache anything,
so a CDN in front of the edge could have cached a tenant-specific body without
varying on the API key. Responses now carry `X-Content-Type-Options: nosniff`,
with `Cache-Control: no-store` on errors and `Cache-Control: private` on cache
hits.

### H5 - Credential failures were not separately observable

Header rejections and authentication failures shared one counter, so credential
stuffing could not be distinguished from clients sending malformed protocol
headers. A dedicated `unauthenticated` counter was added, exposed as
`mcp_usage_unauthenticated_total` and the `mcp.usage.unauthenticated`
OpenTelemetry instrument, both with the existing empty attribute set.

### H6 - Supply-chain gates

`cargo audit` covers advisories and nothing else. Added `deny.toml` and a CI job
enforcing the license allowlist, a crates.io-only source allowlist, yanked-crate
rejection, and a wildcard-dependency ban. Every GitHub Action was pinned from a
mutable tag to a commit SHA, which matters most in `release.yml` because it holds
`id-token: write` and publishes to crates.io. The advisory audit now also runs on
a daily schedule, so an advisory published after the last merge is caught rather
than waiting for the next push.

---

## Verified controls

These are the reasons the audit came back quiet. Each was confirmed by reading
the code, not inferred from documentation.

**Memory safety**

- `#![forbid(unsafe_code)]` in all five crates; zero `unsafe` blocks workspace-wide.
- No `build.rs`, no command execution, no dynamic library loading, no eval-like
  behavior anywhere in the workspace.

**Injection**

- Every SQL statement uses bound parameters. The only raw SQL is a compile-time
  `include_str!` of the bundled schema.
- The Redis claim is a constant Lua script binding only `KEYS[1]`, and that key is
  built from SHA-256 hex digests, so no injection path exists.
- The Redis key prefix is restricted to `[A-Za-z0-9:_-]` and bounded to 64 bytes.

**Secrets and PII**

- API keys are stored and compared as SHA-256 digests, never plaintext. The
  plaintext is reduced to a digest immediately after authentication and is never
  retained on the completion record.
- All four secret-bearing types (`InMemoryTenantStore`, `StripeExporter`,
  `RedisTaskStore`, `PostgresTaskStore`) have hand-written `Debug` impls that
  redact, each with a test asserting the secret is absent from the output.
- Connection URLs are never stored on the store structs. Backend errors are
  collapsed to fixed enum variants before they escape, so no DSN, endpoint, query
  text, or provider response body can reach a log.
- Durable stores persist SHA-256 digests of tenant and task identifiers, never
  plaintext.
- No logging call emits a secret, identifier, request body, or response body.
  Metrics carry fixed names and an empty attribute set, so cardinality and
  privacy risk stay bounded.

**Protocol and trust boundaries**

- Requests declaring a protocol revision older than 2026-07-28, or omitting the
  version, are rejected rather than degraded to body parsing. Without this an
  attacker could claim a legacy version and price an expensive call as a cheap
  one.
- Duplicate `Mcp-*` headers and simultaneous `x-api-key` plus `Authorization` are
  rejected, closing request-smuggling and credential-ambiguity classes.
- A cache entry requires exact agreement between the mirrored method and name
  headers and the JSON-RPC body.
- MRTR continuations are never cached, matching the spec's prohibition.
- A `tasks/get` whose response `taskId` differs from the requested one is denied
  attribution.
- Cached responses retain only `Content-Type`; all other origin headers are
  discarded before storage, so an origin session identifier can never be replayed
  to another caller.
- Cache keys are deterministic: `serde_json`'s `preserve_order` is not enabled, so
  the serialized parameters are key-sorted and stable.

**Resource bounds**

- Request bodies and response observation are both capped at 1 MiB by default,
  with checked arithmetic on the accumulators. An oversized response continues
  streaming but fails toward not charging and not caching.
- The response cache, task-attribution store, and idempotency store are all
  bounded with eviction.
- Billing arithmetic uses `checked_add` and rejects on overflow rather than
  saturating an invoice quantity.

**Transport**

- rustls throughout, with no `danger_accept_invalid_certs`, no custom root store,
  and no certificate-verification override anywhere in the workspace.
- A 30-second per-request timeout on the Stripe client; two-second operation
  timeouts on both durable stores.

---

## Residual risk

Stated plainly, so deployers can make their own call:

- **Origin trust when public cache sharing is enabled.** With
  `with_public_cache_sharing(true)`, correctness depends on every origin behind
  the layer using `cacheScope: "public"` only for results that genuinely do not
  depend on the caller. The default is off precisely because that is an
  assumption about someone else's code.
- **Rate limiting is the deployer's responsibility.** Nothing in these crates
  bounds credential-stuffing attempts. See `SECURITY.md`.
- **API-key entropy is assumed, not enforced.** Keys are compared by SHA-256
  digest, which is a lookup hash, not a password hash. `InMemoryTenantStore`
  accepts any string, so a deployment that admits low-entropy keys is offline
  attackable if the digest table ever leaks.
- **Parsers are tested but not fuzzed.** The SSE splitter, JSON peeks, and base64
  sentinel decoder have unit and conformance coverage, and property testing
  exists in `mcp-usage-core`, but no fuzz targets are defined. This is the most
  useful remaining addition.
- **Two crypto backends.** `ring` and `aws-lc-sys` are both compiled from source.
  Neither has a known advisory; consolidating would reduce the native-code
  surface.

---

## Non-security defect found while verifying H1 (fixed)

Verifying H1 over real HTTP surfaced a separate and more serious bug, which is
recorded here because the audit is what found it.

A server behind `MeterLayer` never served anything from cache, not across tenants
and not even for repeated requests on the same credential. Instrumenting the
metrics showed why: for three billable `tools/call` requests the layer reported
`classified=3` but `billed=0`, `free=0`, and `unrecognized=0`. All three terminal
counters at zero meant `Completion::finish` was never entered at all, so
**billing, cache insertion, and durable-task attribution were silently doing
nothing.**

The cause was that terminal accounting lived only in the `Poll::Ready(None)` arm
of `ObservedBody::poll_frame`. A transport is not obliged to poll a body to
end-of-stream, and hyper stops as soon as the bytes declared by `Content-Length`
have been written, which is the ordinary case for a fixed-length JSON result.
Removing `Content-Length` so the response was chunked produced `billed=3`,
confirming the mechanism. Withholding `is_end_stream` did not help, because hyper
stops on the byte count rather than the flag.

The test suite could not catch this: every test consumed bodies through
`BodyExt::collect`, which always polls to `None` and therefore guarantees an arm
that no real transport guarantees.

Accounting now also runs when the body is dropped, which is the one signal every
transport gives. The completion future is polled once with a no-op waker, which
is sufficient for synchronous recorders and for the default in-memory task store,
whose futures never yield. A store performing real I/O may still be pending; that
case increments `record_failures` rather than passing silently. Two regression
tests cover it: one drives a body by hand and drops it without ever observing
end-of-stream, and one asserts a fully consumed body is still accounted for
exactly once.

This was not a security defect on its own. It failed in the safe direction, since
an entry that is never cached cannot be served to the wrong tenant. Its relevance
to this audit is the reverse: fixing it is what makes the cache reachable, so the
H1 isolation controls became load-bearing only after this change. Cross-tenant
isolation was re-verified over real HTTP afterwards, with a second tenant still
reaching the origin rather than receiving the first tenant's cached body.
