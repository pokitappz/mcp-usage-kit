# UsageKit conformance suite

The versioned JSON vectors under
[`crates/mcp-usage-core/conformance`](../crates/mcp-usage-core/conformance/v1/cases.json)
define the observable accounting contract independently of Rust, Tower, Stripe,
or any storage backend.

An implementation conforms to version 1 when it produces the expected verdict,
unit quantity, free reason, and idempotency key for every vector. Implementations
may add private behavior, but must fail toward free when a response is malformed
or cannot be attributed safely.

## Compatibility policy

- Existing vectors are immutable after release.
- New cases that clarify existing rules may be appended within the same schema.
- A change to field meaning or expected behavior requires a new schema directory.
- Published release tags are the source of truth for reproducible certification.

The Rust reference test is compiled and run by workspace CI. Other languages can
consume the JSON directly without reproducing the Rust type model.
