# Stripe Billing Meter Events API contract

Verified 2026-07-31 against Stripe's current documentation and the current
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

Values must be whole numbers. Timestamps must be within the past 35 calendar
days and no more than 5 minutes in the future. Stripe enforces identifier
uniqueness within a rolling 24-hour window. This exporter pre-aggregates events,
uses globally unique stable identifiers, and retries the same identifier after
an ambiguous failure.

The v1 endpoint supports 1,000 live-mode calls per second. Sandbox calls count
toward Stripe's basic API limit. Stripe also offers API v2 meter events and a
high-throughput meter event stream. This crate uses v1.

Source: [record usage for billing](https://docs.stripe.com/billing/subscriptions/usage-based/recording-usage-api).

## HTTP client

The workspace uses `reqwest` 0.13.4 with Rustls, no native TLS, and a per-request
30-second timeout. Export errors omit response bodies, endpoint URLs, customer
identifiers, and API keys so provider diagnostics cannot leak through logs.
Endpoint overrides are restricted to IP-addressed loopback test servers; other
custom hosts are rejected before a request is sent.

Source: [`reqwest` 0.13.4 documentation](https://docs.rs/reqwest/0.13.4/reqwest/).
