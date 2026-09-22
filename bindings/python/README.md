# mcp-usage-kit (Python)

Protocol-correct usage accounting for MCP servers, in process.

```sh
pip install mcp-usage-kit
```

```python
import mcp_usage_kit as usage

meter = usage.Meter(usage.PriceBook(default_units=1, names={"sum": 7}))

charge = meter.decide(method="tools/call", name="sum", response=usage.delivered())
charge.billable         # True
charge.units            # 7
charge.idempotency_key  # None
```

## Why this exists

The [sidecar](../../crates/mcp-usage-edge) meters an MCP server from outside,
over HTTP, in any language. This meters one from inside, with no extra hop.

Both run the same Rust engine, so a `FastMCP` server in Python bills identically
to a Rust one **by construction** rather than by careful reimplementation.
`mcp-usage-core` is pure, synchronous and free of I/O, which is what makes a
second language cheap: this package is a translation layer and nothing else.

## What gets billed

Usage is recorded on terminal delivery, not on requests. Counting requests
overbills in four places that all produce disputes:

| Exchange | Request counting | This |
|---|---|---|
| Multi-round-trip call | billed per round trip | billed once |
| Task creation, then polls | billed per poll | billed once, on completion |
| JSON-RPC error | billed | free, `protocol_error` |
| `tools/list` and discovery | billed | free, `discovery` |

A free charge always says why, so "my bill is lower than my request count" has
a real answer rather than a shrug.

## Envelope helpers

A framework hands its middleware a result object, not a JSON-RPC envelope.
These do that translation in one tested place, because hand-writing
`{"result": {"resultType": ...}}` puts a silent billing change one typo away.

| Helper | Meaning |
|---|---|
| `delivered()` | Work was delivered |
| `awaiting_input()` | An interim round trip, waiting on the caller |
| `failed(code, message)` | A JSON-RPC error |
| `task_created(task_id, status)` | Work accepted as a durable task |
| `task_poll(task_id, status)` | A poll of a durable task |

If you already hold the raw response, pass it directly: `decide` accepts a
mapping or a JSON string.

`name` may be the raw `Mcp-Name` header value. The sentinel form
(`=?base64?...?=`) is decoded before pricing, because an encoded value matches
nothing in the price book and would otherwise charge every non-ASCII-named
tool the default instead of its own price.

## Durable tasks

A `tasks/get` carries no name of its own, so a completed task cannot be priced
without the call that created it. Supply it and the completion bills the
originating tool's price, once:

```python
charge = meter.decide(
    method="tasks/get",
    response=usage.task_poll("task-1", "completed"),
    task_origin=("tools/call", "report"),
)
charge.units            # the price of `report`
charge.idempotency_key  # "task-1"
```

**Deduplicate on `idempotency_key`.** A terminal task reports `completed` to
every later poll, so without it a slow task bills once per poll. Omit the
origin and the completion is recorded free with
`reason == "missing_task_attribution"` rather than guessed at.

## Quota and spend caps

```python
outcome = usage.assess_limits(
    committed_units=8, requested_units=2, max_units=10
)
outcome.allowed  # True: exact boundaries are allowed
```

A limit rejects only when the new total would be *greater*. `reason` is one of
`quota_exceeded`, `spend_cap_exceeded` or `usage_unrepresentable` - the same
codes the sidecar returns over HTTP, so moving between the two does not change
what you branch on.

## FastMCP

[`examples/fastmcp_metering.py`](examples/fastmcp_metering.py) is a working
middleware:

```
billed sum_numbers: 7 units
billed cheap_probe: 0 units
free   always_fails: protocol_error
```

Note that a zero-priced tool is still *billable*: it delivered work, it just
costs nothing. That is the supported way to expose a loss leader without
carving it out of the billing path.

## Conformance

`tests/test_conformance.py` runs the same
[`conformance/v1/cases.json`](../../crates/mcp-usage-core/conformance/v1/cases.json)
vectors the Rust reference test runs. The engine underneath is already the
reference, so what those vectors actually prove is the translation: that a
Python mapping becomes the same JSON the Rust edge sees, that method and name
arrive intact, that task origins are wired through, and that free reasons
surface under their stable wire names.

## Building from source

```sh
pip install maturin pytest
maturin develop --release
pytest tests/
```

The extension is built against the stable ABI (`abi3-py39`), so one wheel per
platform covers Python 3.9 and later.
