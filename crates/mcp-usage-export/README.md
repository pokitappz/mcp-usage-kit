# mcp-usage-export

Failure-isolated billing exporters for MCP usage. The pipeline records usage
synchronously, aggregates concurrent events, and preserves quantities and
provider event identifiers across retries. Export failure never changes an MCP
response.

The optional `stripe` feature exports pre-aggregated integer quantities to
Stripe Billing Meter Events. Aggregates older than Stripe's accepted timestamp
window are retained in a bounded dead letter queue without rewriting their
timestamps, and can be drained for reconciliation.

[API documentation](https://docs.rs/mcp-usage-export) | [Project site](https://pokitappz.github.io/mcp-usage-kit/)

Licensed under Apache-2.0.
