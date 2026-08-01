# mcp-usage-export

Failure-isolated billing exporters for MCP usage. The pipeline records usage
synchronously, aggregates concurrent events, and preserves quantities and
provider event identifiers across retries. Export failure never changes an MCP
response.

The optional `stripe` feature exports pre-aggregated integer quantities to
Stripe Billing Meter Events. Aggregates outside Stripe's accepted timestamp
window and events synchronously rejected as permanently invalid are retained in
a bounded dead letter queue without rewriting their timestamps. Applications
can also retain usage rejected during Stripe's asynchronous processing through
`StripeExporter::quarantine_async_rejection`. All retained data can be drained
for reconciliation. When a transient response interrupts a batch, process-local
progress skips events with confirmed success and requires the identical batch
on retry.

[API documentation](https://docs.rs/mcp-usage-export) | [Project site](https://pokitappz.github.io/mcp-usage-kit/)

Licensed under Apache-2.0.
