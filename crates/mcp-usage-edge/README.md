# mcp-usage-edge

Metering sidecar for MCP servers written in any language.

The rest of this workspace meters an MCP server from inside the process, which
means the server has to be written in Rust. This binary puts the same
`MeterLayer` in front of an arbitrary upstream over HTTP instead. A `FastMCP`
server in Python, a TypeScript server, or anything else gets identical billing
semantics without changing a line of its code.

```
  [ agent ] --MCP--> [ mcp-usage-edge ] --MCP--> [ your MCP server ]
                      meters, prices,             unchanged,
                      enforces quota              any language
```

The metering decisions still come from `mcp-usage-core`, unchanged and
unreachable from here. The sidecar cannot bill differently from the library;
the only thing it adds is transport.

## Why a sidecar rather than a proxy in your stack

Counting HTTP requests is the obvious way to meter an MCP server and it is
wrong in four places that all produce disputes:

| Exchange | Request counting | This sidecar |
|---|---|---|
| Multi-round-trip call | billed per round trip | billed once |
| Task creation, then polls | billed per poll | billed once, on completion |
| JSON-RPC error | billed | free |
| `tools/list` and discovery | billed | free |

Usage is recorded on terminal delivery. See the [billing rule](../../README.md)
in the workspace README for the full table.

## Quick start

```sh
cp edge.example.toml edge.toml          # edit upstream.url and your prices
export TENANT_ACME_KEY="$(openssl rand -base64 32)"
mcp-usage-edge edge.toml
```

Point your MCP clients at the sidecar instead of at your server. Nothing else
changes.

With `exporter.kind = "log"` every flush prints the aggregate, which is how you
verify pricing before wiring a billing provider:

```
INFO mcp_usage_export: MCP billing usage identifier=mcp_usage_01bc… meter=mcp_units units=7
```

## Configuration

Every knob the library exposes through a builder method is a named field in the
TOML file with the same default. See `edge.example.toml` for the annotated
reference. Two of them are worth reading before you deploy.

**`edge.credential_forwarding`** defaults to `true` here, unlike the library,
where it defaults to `false`. In a sidecar the upstream is your own
already-authenticated MCP server, so dropping the caller's credential would
silently break its auth. Set it to `false` only when the upstream is in a
different trust domain and should never see the caller's secret.

**`edge.strict_protocol_version`** refuses clients older than MCP 2026-07-28
rather than parsing their body to classify them. It is off by default, matching
the library: a sidecar whose promise is "change nothing" must not start
returning `400` to older clients. Turn it on once you know every client is
current, and it saves a JSON parse per request. `FastMCP` 4.0.5 sends the
mirrored headers, so it meters correctly either way.

Tenant keys should come from the environment via `api_key_env`, not from
`api_key`, which exists for local development. Keys are checked for strength at
startup, so a weak or duplicated key fails before the listener opens rather
than at the first request.

## Running against a control plane

With a `[control_plane]` section the sidecar stops reading tenants from the file
and pulls them from the plane instead, along with prices, limits and the
authoritative usage counters. Usage goes back the same way when
`exporter.kind = "control-plane"`.

```toml
[control_plane]
url = "https://plane.example.com"
token_env = "PLANE_EDGE_TOKEN"
refresh_interval_seconds = 30
max_stale_seconds = 900
enforce_quota = true

[exporter]
kind = "control-plane"
```

The design rests on one promise: **the plane being down never fails a customer's
MCP call.** The tenant cache keeps serving and the exporter keeps its batch
pending rather than dropping it, so an outage costs freshness and delayed
invoicing, not availability. The one deliberate exception is `max_stale_seconds`:
past that the sidecar fails closed, because serving hours-old revocations is
worse than refusing.

Revocation is absence. A revoked key stops appearing in the snapshot and stops
authenticating within one refresh interval, with no restart.

A credential that does not verify is not fatal on its own: the call falls back
to the ordinary quota check, so a tenant that is inside its cap is served even
while it keeps attaching a spent credential. Only if quota also refuses is the
call rejected, and then the answer names what was wrong with the credential
rather than the generic "payment required".

The credential stops at the gate. It carries the payer's identifier and a
settlement proof, and the upstream has no business seeing either.

Quota is admission-only and resolves to one refresh interval: a tenant already
over its cap is refused with `429` and a static code, and nothing is metered for
that call. A tenant can still overshoot by whatever it spends inside one window.
The alternative would be a synchronous reservation call to the plane on every
MCP request, which would put the plane's availability directly in front of
customer traffic, and avoiding that is the whole point.

```
HTTP/1.1 429 Too Many Requests
{"error":"quota_exceeded","retryable":false}
```

## Accepting payment instead of refusing (MPP)

With an `[mpp]` section an over-quota call is *priced* rather than refused. The
sidecar answers `402 Payment Required` with a `WWW-Authenticate: Payment`
challenge, the agent pays, retries, and gets the call plus a `Payment-Receipt`.

This is the "Payment" HTTP authentication scheme from
[draft-ryan-httpauth-payment-01](https://datatracker.ietf.org/doc/html/draft-ryan-httpauth-payment-01),
which is what MPP is built on. It is an *inbound* protocol: there is nothing to
export to and no discovery step before the 402, which is why it lives at the
edge rather than in a control plane.

```
agent -> sidecar                     402 + WWW-Authenticate: Payment
                                          Cache-Control: no-store
                                          application/problem+json
agent pays out of band
agent -> sidecar  Payment-Authorization: Payment <base64url>
                                     200 + Payment-Receipt: <base64url>
```

The credential arrives in `Payment-Authorization`, not `Authorization`. The
draft's optional `header` parameter exists for exactly this, and the sidecar
always uses it: `Authorization` already carries the tenant's own bearer key on
its way to the upstream, and overwriting it would break the customer's
authentication in order to fund a payment.

### What is checked before anything settles

| Check | Why |
|---|---|
| HMAC binding over every challenge parameter | Otherwise an agent mints its own challenge and sets its own price |
| `realm` and `method` match this server | A credential for someone else is not payment here |
| `expires` | A challenge is time-bound |
| RFC 9530 body digest | Binds a credential to the exact call it paid for, so it cannot be moved to a more expensive one |
| Priced identity in `opaque` | The digest binds the body, but the amount comes from the call's identity, so a credential bought under a cheap name cannot redeem an expensive one |
| Amount re-priced now | A price change between issue and redemption cannot be ridden out |
| Single-use reservation | The draft requires a proof be spendable exactly once, and concurrent presentations settle at most once. A proof the method refuses is released again, so nobody can burn a slot with a junk payload |

Only then is the payment method consulted. Each check maps to a registered
problem type under `https://paymentauth.org/problems/`, so an agent branches on
a URI rather than parsing prose.

### Verification

The scheme is method-agnostic and leaves verification to each payment method's
own specification, against its own network. The sidecar therefore asks a
facilitator: one service that holds the network credentials and answers whether
a proof settled and for how much.

**The facilitator request and response shape is this sidecar's own contract, not
part of MPP**, which does not define one. It is deliberately small so fronting
an existing facilitator is a thin adapter:

```json
POST <url>
{"method":"tempo","intent":"charge","challenge_id":"...","source":"did:...",
 "request":{"amount":"1000","currency":"usd","recipient":"acct_123"},
 "payload":{ ...method-specific proof... }}

200 {"settled":true,"reference":"0xabc..."}
200 {"settled":false,"reason":"insufficient"}
```

A facilitator that times out, is unreachable, or answers anything else leaves
the payment unproven and the call refused. Failing open would make an outage at
the facilitator a free-service coupon.

Implementing a method natively instead means implementing that method's spec
against its settlement layer and supplying a `PaymentMethod`.

### Pricing

A challenge is priced from the tenant's own price book: the units the call would
be metered at, times the tenant's unit price, converted to the currency's minor
unit and **rounded up**. Serving below cost is the worse failure.

Payment is about admission only. Once admitted, a paid call is ordinary traffic
and is metered exactly like a prepaid one.

## What it does not do

- **No quota without a plane.** A statically configured sidecar has no
  authoritative counters, so the gate is disabled and admits everything. The
  same applies to `[mpp]`, which is refused without `[control_plane]`.
- **Single-instance replay protection.** Spent proofs are remembered in
  process. A horizontally scaled deployment needs a shared store, the same way
  durable task attribution does.
- **No in-process context.** The sidecar sees the MCP wire protocol and nothing
  else. An application that wants to price on its own internal state should
  embed `mcp-usage-kit` directly.
- **One extra hop.** Responses stream through rather than being buffered, so an
  SSE stream stays a stream, but the hop is real.

## Verification

`cargo test -p mcp-usage-edge` runs the unit tests and an end-to-end suite that
stands up a real upstream server, a real sidecar listener and a real client,
then asserts every row of the billing rule table across the proxy hop,
including that a repeat poll of a terminal task stays idempotent and that an
unreachable upstream returns `502` and bills nothing.

The sidecar has also been run against an unmodified `FastMCP` 4.0.5 server: tool
discovery stayed free, a zero-priced tool stayed free, and a priced tool billed
exactly its configured units.
