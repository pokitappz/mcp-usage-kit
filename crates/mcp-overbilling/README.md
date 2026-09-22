# How much does request counting overbill MCP?

Every usage meter aimed at MCP servers today bills HTTP requests. MCP does not
deliver one piece of work per request, so that number is wrong. This measures
how wrong.

```sh
cargo run -p mcp-overbilling
```

```
Units invoiced for identical MCP traffic

Category            reqs  correct   requests  requests+name   delivery
----------------------------------------------------------------------
Discovery              7        0          7              7          0
Multi-round-trip       8       35          8            155         35
Task polling          12       40         12             80         40
Errors                 9        5          9            135          5
Retries                9       72          9             76         72
----------------------------------------------------------------------
Total                 45      152         45            453        152

requests: 0.30x the correct invoice, underbilling by 70%.
requests+name: 2.98x the correct invoice, overbilling by 198%.
delivery: bills exactly the delivered work.
```

`--json` gives the same numbers machine-readably. `--strict` exits non-zero if
this project's own meter disagrees with the declared answers.

## Reading this honestly

**`correct` is a claim, and it is written down where you can argue with it.**
Every scenario in [`scenarios/v1.json`](scenarios/v1.json) carries a
`groundTruthRationale` saying why that number is right. If you think one is
wrong, that field is the thing to disagree with, and the number moves for both
meters when it changes.

**Two request counters, not one.** `requests` charges a flat rate per HTTP
request, which is what an API gateway or HTTP middleware can do - it has no
idea which tool a request was for. `requests+name` reads `Mcp-Name` and applies
the customer's own price book per request, which is the best a request counter
could possibly do. **The headline number is the second one.** Beating the
weaker version would prove nothing.

Notice that the flat counter *under*bills by 70% while the named one overbills
by 198%. A protocol-blind meter is not merely expensive; it is uncorrelated. It
cannot price a `tasks/get` at all, because the poll that finally delivers a
durable task carries no name - only the call that created it did.

**The suite contains traffic request counting gets right.** `ordinary-billable-call`
and `three-ordinary-calls` are plain successful calls where both meters land on
the same invoice, and a test fails if fewer than two such scenarios exist. A
comparison assembled only from traffic that favours one answer is marketing.

**It tests this project too.** `delivery` is
[`mcp-usage-core`](../mcp-usage-core), and `terminal_delivery_matches_every_declared_ground_truth`
fails if it ever drifts from the declared answers. The numbers above are not
"what our engine does" - they are what we claim is correct, with our engine
held to it like anything else. Both halves of that were checked by breaking
them: a wrong ground truth and a broken task deduplication each fail the suite.

## Where the money goes

| Category | What happens | Why counting requests gets it wrong |
|---|---|---|
| Discovery | `tools/list`, `resources/list`, reconnects | Bills the cost of connecting. Scales with network flakiness, not with value. |
| Multi-round-trip | One call that stops to ask a question | Bills each turn. The more carefully a tool converses, the more it costs. |
| Task polling | A durable task polled until ready | The client picks the poll interval, so the client sets the invoice. |
| Errors | The server fails, the agent retries | An unreliable server earns more than a reliable one. |
| Retries | A finished task looked at again | The same delivery invoiced once per look. A client that caches badly pays more. |

The worst of these is not the multiple. It is that in three of the five, the
invoice is driven by something the customer did not choose and cannot predict -
a retry loop, a poll interval, an outage. That is what generates disputes.

## Adding another meter

Implement `Meter` and put it in the list in `src/main.rs`:

```rust
pub trait Meter {
    fn name(&self) -> &str;
    fn bill(&mut self, scenario: &Scenario) -> u64;
}
```

`bill` receives the whole scenario in order, because a meter with memory needs
it: deduplication and task attribution both depend on what earlier exchanges
said.

If you run a metering product and think this misrepresents it, the fix is a PR
adding your rules as a `Meter`. That is a better outcome than us guessing.

## What this does not do yet

It replays scenarios in-process. It does not drive a live vendor endpoint over
HTTP, so it measures *rules*, not deployments. The scenario file has everything
needed to replay as real traffic - method, name and response body per exchange -
so that is an adapter rather than a rewrite, but it is not written.
