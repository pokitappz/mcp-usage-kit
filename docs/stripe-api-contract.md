# Stripe Billing Meter Events API contract

Verified 2026-08-01 against Stripe's current documentation and the current
`reqwest` release. Re-check this note before changing the exporter.

## Endpoint and authentication

- The exporter targets `POST /v1/billing/meter_events` at
  `https://api.stripe.com`.
- Stripe authenticates API v1 calls with HTTP Basic authentication. The secret
  key is the username and the password is empty.
- Test and live mode use the same host. `sk_test_` keys select test mode and
  `sk_live_` keys select live mode. Restricted keys are also supported when they
  have the required Billing permissions.
- The exporter does not send `Stripe-Version`, so Stripe applies the API version
  configured for the account. The v1 meter event request shape used here is
  documented across currently published API versions.

Sources: [Stripe API authentication](https://docs.stripe.com/api/authentication),
[create a billing meter event](https://docs.stripe.com/api/billing/meter-event/create).

## Wire format and operational constraints

The request is form encoded with `event_name`, `payload[stripe_customer_id]`,
`payload[value]`, `identifier`, and `timestamp`. The meter must use Stripe's
default customer and value payload keys. If a dashboard-configured meter uses
custom payload key overrides, this exporter needs matching configuration before
it can submit to that meter.

Stripe accepts decimal values; this exporter deliberately sends the nonnegative
integer subset produced by its usage accumulator. Timestamps must be within the
past 35 calendar days and no more than 5 minutes in the future. Stripe enforces
identifier uniqueness within a rolling 24-hour window. This exporter
pre-aggregates events, uses globally unique stable identifiers, and retries the
same identifier after an ambiguous failure.

An aggregate strictly older than the 35-day window or more than five minutes in
the future is quarantined without changing its timestamp. A synchronous client
error that describes a permanently invalid individual event is also
quarantined. Valid aggregates later in a mixed batch still export.

Authentication and permission failures, request timeouts, external dependency
failures, rate limits, transport failures, and server errors remain retryable.
This avoids dead-lettering an otherwise valid event because of account
configuration, availability, or throttling.

If a retryable failure follows confirmed successful responses in the same
batch, the exporter retains process-local progress and requires the identical
batch on retry. It skips confirmed events and resumes at the first unresolved
event. A request cancelled while an event is in flight still has an ambiguous
provider outcome, so that event is retried with the same identifier. Process
loss discards partial progress; applications that need crash-durable
reconciliation must keep their own source records.

`StripeExporter` retains up to 1,024 aggregates by default, deduplicated by
identifier, and exposes them through `take_dead_letters` for reconciliation.
Retention is configurable with `with_dead_letter_capacity`; evictions are
observable through `dropped_dead_letters`.

## Asynchronous processing failures

Stripe processes Meter Events asynchronously. A successful create response is
not final proof that the event was accepted for billing. Stripe reports later
failures through the `v1.billing.meter.error_report_triggered` and
`v1.billing.meter.no_meter_found` thin events. The application must configure an
event destination, verify the Stripe signature, retrieve the thin event, and
correlate its error with the original source usage.

This runtime-neutral crate does not own a webhook server or persistent source
records. After verification and correlation, the application passes the
original aggregate to `StripeExporter::quarantine_async_rejection`. The entry is
then bounded, deduplicated, observable, and drained through the same dead letter
API as synchronous failures. Stripe may include only a sample of invalid events
in an error report, so applications should preserve their own audit trail for
complete reconciliation.

The v1 endpoint supports 1,000 live-mode calls per second. Sandbox calls count
toward Stripe's basic API limit. Stripe also offers API v2 meter events and a
high-throughput meter event stream. This crate uses v1.

Sources: [record usage for billing](https://docs.stripe.com/billing/subscriptions/usage-based/recording-usage-api),
[handle Meter Event errors](https://docs.stripe.com/billing/subscriptions/usage-based/recording-usage-api#handle-meter-event-errors).

## HTTP client

The workspace uses `reqwest` 0.13.4 with Rustls, no native TLS, and a per-request
30-second timeout. Export errors omit response bodies, endpoint URLs, customer
identifiers, and API keys so provider diagnostics cannot leak through logs.
Endpoint overrides are restricted to IP-addressed loopback test servers; other
custom hosts are rejected at construction, before the exporter is usable.

The client refuses redirects. The endpoint allowlist checks the URL the exporter
is configured with, so following a 3xx would move the request to a host that was
never checked; `reqwest` strips `Authorization` across hosts, which protects the
secret key but not the form body carrying `payload[stripe_customer_id]`. Stripe's
v1 API does not redirect, so no legitimate response is affected, and a redirect
now surfaces as an ordinary non-2xx status.

Source: [`reqwest` 0.13.4 documentation](https://docs.rs/reqwest/0.13.4/reqwest/).
