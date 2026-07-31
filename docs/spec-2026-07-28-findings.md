# MCP 2026-07-28: verified findings that shape the meter

Checked against the specification on 2026-07-31. Re-check these findings when a
new protocol revision is published.

Sources:

- [Transports overview](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports)
- [Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
- [Multi Round-Trip Requests](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr)
- [Tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)
- [Caching](https://modelcontextprotocol.io/specification/2026-07-28/server/utilities/caching)
- [Tasks extension](https://modelcontextprotocol.io/extensions/tasks/overview)

## 1. Headers mirror the body, and the body wins

Streamable HTTP mirrors `method` into `Mcp-Method` and `params.name` / `params.uri`
into `Mcp-Name`, "so that intermediaries (load balancers, gateways, observability
tooling) can route and inspect requests without parsing the body." Both are REQUIRED
for compliance, alongside `MCP-Protocol-Version`.

But the body is the source of truth. Servers that process the body **MUST** reject a
header/body mismatch with `400` and JSON-RPC error `-32020` (`HeaderMismatch`),
explicitly "to prevent potential security vulnerabilities when different components in
the network rely on different sources of truth (e.g., a load balancer routing on the
header value while the MCP server executes based on the body value)."

**Consequence for an intermediary that meters on headers.** The spec addresses this
case directly and imposes a guard:

> Intermediaries that enforce policy based on mirrored headers (e.g., routing or
> rate-limiting by tenant) **SHOULD** verify that the `MCP-Protocol-Version` header
> indicates a version that requires header-body validation. If the version is older or
> the header is absent, the intermediary **SHOULD** reject the request rather than
> trusting unvalidated header values.

So the header fast path is legitimate, but only behind a version gate that **rejects**
rather than falls back. A meter that trusts `Mcp-Name` on a pre-2026-07-28 request is
billing on an attacker-chosen value.

`Mcp-Name` may also arrive Base64 sentinel-encoded as `=?base64?{value}?=` (markers are
lowercase and case-sensitive) whenever the name is not safely representable in ASCII.
Anything matching a price book on tool name **MUST** decode first, or it bills the wrong
line item.

## 2. There is no gateway-readable MRTR correlator, and we do not need one

`requestState` is "an opaque string meaningful only to the server. Clients **MUST NOT**
inspect, parse, modify, or make any assumptions about its contents." Servers **MUST**
integrity-protect it (HMAC or AEAD) whenever it influences authorization or business
logic; the spec's own example value is literally `"AEAD-protected blob"`. It is also
optional, and the JSON-RPC `id` **MUST** differ between the initial request and the
retry.

This prevents a third-party proxy from using `requestState` as a chain correlator,
because only the origin has the integrity-protection key.

Chain correlation is not required for billing. The spec gives a direct test for
"this request is a continuation" and uses it itself in the caching rules: requests
"carrying `inputResponses` or `requestState`" are continuations. Both are **body**
fields under `params`, mirrored into no header.

MRTR is confined to exactly three methods. Servers **MAY** return `InputRequiredResult`
on `prompts/get`, `resources/read`, and `tools/call`, and **MUST NOT** on any other
request. So the body peek is bounded to three method values, all identifiable from
`Mcp-Method` alone.

Chains can exceed two round trips: servers "**MAY** choose to return an
`InputRequiredResult` on multiple attempts at the same request."

## 3. The correct billing rule: charge on terminal delivery, not on request

Every result carries a `resultType`. `"complete"` is a finished result;
`"input_required"` is an interim one. Charging when `resultType == "complete"` gives,
with no correlator and no state:

- An MRTR chain of any length bills exactly once, because only its last round trip is
  `complete`.
- An abandoned chain bills zero, because it never reaches `complete`.
- A task's polls bill zero and its terminal `tasks/get` bills once, because only the
  terminal poll carries the final result.
- Failures are naturally distinguishable, since protocol errors are JSON-RPC errors
  rather than results.

The resulting invoice counts delivered work instead of protocol exchanges.

The cost is that the signal lives in the **response**, so the meter observes responses,
not just requests. For `Content-Type: application/json` that is one shallow parse. For
`text/event-stream` the final SSE event carries the response, so the meter must read to
the end of the stream. In-process (`mcp-usage-kit`) this is free; in a proxy it means not
billing until the stream terminates.

## 4. Tasks change the shape but not the rule

Under the `io.modelcontextprotocol/tasks` extension a `tools/call` can return a durable
task handle; the client then drives `tasks/get`, `tasks/update`, and `tasks/cancel`.
`tasks/list` was removed, since without sessions listing every task is not safe to
expose. Task creation is server-directed: the client advertises the extension and the
server decides.

Under the terminal-delivery rule this needs no special case. Polls are non-terminal and
therefore free; the terminal state is billable. A meter that counts requests bills a
10-minute job polled every 2s about 300 times.

## 5. Caching is a correctness hazard before it is an optimization

`ttlMs` is a freshness hint in milliseconds, analogous to `Cache-Control: max-age`;
servers **MUST** provide a value `>= 0`, and absent means treat as `0`. `cacheScope` is
`"public"` or `"private"`.

The key rule for a shared gateway cache is on `"private"`: "Cached responses
**MAY** be reused for the same authorization context. Caches **MUST NOT** be shared
across authorization contexts (e.g. a different access token requires a different
cache)." A multi-tenant cache keyed without the authorization context is a cross-tenant
data leak, not a performance bug.

Two further rules:

- Cacheable results are `server/discover`, `tools/list`, `prompts/list`,
  `resources/list`, `resources/templates/list`, and `resources/read`. Note that
  `resources/read` is cacheable and is **also** MRTR-capable.
- Results produced through MRTR, "that is, requests carrying `inputResponses` or
  `requestState`, **MUST NOT** be cached." The same body peek from finding 2 gates the
  cache and the meter.
- The cache key is the method plus the parameters that affect the result, including the
  pagination `cursor`. `resultType: "input_required"` results are never cacheable.

## 6. Two shapes the original plan did not account for

- **`subscriptions/listen`** returns a long-lived SSE stream carrying change
  notifications. It is neither a discrete call nor a task, so per-call billing does not
  describe it. Needs an explicit rule: currently treated as free, metered on duration
  later if it turns out to cost real money to hold open.
- **`x-mcp-header`** lets a server mirror chosen tool parameters into
  `Mcp-Param-{Name}` headers. A server can promote a
  billing-relevant argument into a header and let the meter price on it without ever
  touching the body.

## 7. SDK

`rmcp` is at **3.1.0** (Apache-2.0, MSRV 1.88), not the 2.2.0 that a stale docs.rs
listing reported. Relevant features: `transport-streamable-http-server`, `tower`, and
`request-state`, the last of which pulls `hmac` + `sha2` + `base64` to satisfy the
finding-2 integrity requirement.
