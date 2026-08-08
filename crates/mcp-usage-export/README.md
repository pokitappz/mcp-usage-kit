# mcp-usage-export

Failure-isolated billing exporters for MCP usage. The pipeline records usage
synchronously, aggregates concurrent events, and preserves quantities and
provider event identifiers across retries. Export failure never changes an MCP
response.

`MeterEventExporter` connects the pipeline to an application-supplied
`MeterEventProvider` without adding an HTTP client or provider SDK. The provider
receives only unresolved aggregates in original order and returns one ordered
outcome per aggregate. Confirmed events are skipped on retry, while ambiguous
and retryable events retain their stable identifiers. Permanent and verified
asynchronous rejections use a bounded, identifier-deduplicated dead letter queue
that applications can drain for reconciliation.

Provider implementations own transport, authentication, validation, timestamp
rules, and wire encoding. Outcome and batch-error codes must be static,
low-cardinality categories containing no secrets or customer data.

## Provider implementation example

The following provider-shaped example shows the boundary an adapter owns. The
payload fields are deliberately generic, but they cover the stable meter
identity, customer, quantity, meter, and timestamp data that a hosted billing
API might require. Replace `send` with the application's authenticated
transport and map its responses to the ordered outcomes.

```rust,ignore
use mcp_usage_export::{
    AggregatedUsage, MeterEventOutcome, MeterEventProvider, MeterEventProviderError,
    MeterEventProviderFuture,
};

#[derive(Debug)]
struct ExampleMeterProvider;

#[derive(Debug)]
struct ExampleMeterPayload {
    identifier: String,
    customer_id: String,
    meter: String,
    units: u64,
    timestamp: u64,
}

impl ExampleMeterProvider {
    async fn send(
        &self,
        payloads: Vec<ExampleMeterPayload>,
    ) -> Result<Vec<MeterEventOutcome>, MeterEventProviderError> {
        // Send payloads through the application's provider client here.
        // Preserve payload order when translating provider responses.
        Ok(payloads
            .into_iter()
            .map(|payload| {
                let _ = payload;
                MeterEventOutcome::Accepted
            })
            .collect())
    }
}

impl MeterEventProvider for ExampleMeterProvider {
    fn submit<'a>(&'a self, batch: &'a [AggregatedUsage]) -> MeterEventProviderFuture<'a> {
        Box::pin(async move {
            let payloads = batch
                .iter()
                .map(|usage| ExampleMeterPayload {
                    identifier: usage.identifier.clone(),
                    customer_id: usage.customer_id.clone(),
                    meter: usage.meter.clone(),
                    units: usage.units,
                    timestamp: usage.timestamp,
                })
                .collect();
            self.send(payloads).await
        })
    }
}
```

The real `send` implementation must return one outcome for every payload. Map
temporary transport or provider availability failures to
`RetryableFailure { code: "unavailable" }`, event-specific permanent failures
to `PermanentRejection { code: "invalid_event" }`, and batch-wide failures to a
static `MeterEventProviderError` such as `"unavailable"`. Keep provider-side
idempotency keyed by `AggregatedUsage::identifier` across ambiguous retries.

Wrap the provider in the normal pipeline:

```rust,ignore
use mcp_usage_export::{BillingPipeline, MeterEventExporter};

let billing = BillingPipeline::new(MeterEventExporter::new(ExampleMeterProvider));
```

[API documentation](https://docs.rs/mcp-usage-export) | [Project site](https://pokitappz.github.io/mcp-usage-kit/)

Licensed under Apache-2.0.
