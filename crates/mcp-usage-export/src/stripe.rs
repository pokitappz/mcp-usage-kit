//! Stripe Billing Meter Events exporter.

use std::fmt;
use std::time::Duration;

use super::{AggregatedUsage, BatchExporter, ExportError, ExportFuture};

const DEFAULT_ENDPOINT: &str = "https://api.stripe.com/v1/billing/meter_events";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Exports pre-aggregated usage through Stripe's v1 Billing Meter Events API.
///
/// Every request includes the stable [`AggregatedUsage::identifier`]. Stripe
/// deduplicates meter-event identifiers for at least 24 hours, making a retry
/// safe when the transport fails after Stripe accepted the request.
pub struct StripeExporter {
    client: reqwest::Client,
    secret_key: String,
    endpoint: String,
}

impl StripeExporter {
    /// Construct an exporter targeting Stripe's production API endpoint.
    #[must_use]
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            secret_key: secret_key.into(),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
        }
    }

    /// Override the endpoint for an IP-addressed loopback test server.
    ///
    /// Export rejects other custom hosts so a configuration error cannot send
    /// the Stripe secret to an unrelated server.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

impl fmt::Debug for StripeExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StripeExporter")
            .field("secret_key", &"[REDACTED]")
            .field("endpoint", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl BatchExporter for StripeExporter {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            if !endpoint_is_allowed(&self.endpoint) {
                return Err(ExportError::Provider(
                    "Stripe endpoint must be api.stripe.com or a loopback test server".to_owned(),
                ));
            }
            for usage in batch {
                let form = form_fields(usage);
                let response = self
                    .client
                    .post(&self.endpoint)
                    .basic_auth(&self.secret_key, Some(""))
                    .form(&form)
                    .timeout(REQUEST_TIMEOUT)
                    .send()
                    .await
                    .map_err(|error| sanitized_transport_error(&error))?;
                if !response.status().is_success() {
                    let status = response.status();
                    return Err(ExportError::Provider(format!(
                        "Stripe returned HTTP {status}"
                    )));
                }
            }
            Ok(())
        })
    }
}

fn endpoint_is_allowed(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    if url.scheme() == "https" && url.host_str() == Some("api.stripe.com") {
        return true;
    }
    matches!(url.scheme(), "http" | "https")
        && url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback())
}

fn sanitized_transport_error(error: &reqwest::Error) -> ExportError {
    let category = if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_request() {
        "request construction failed"
    } else if error.is_body() {
        "request body failed"
    } else {
        "transport failed"
    };
    ExportError::Provider(format!("Stripe {category}"))
}

fn form_fields(usage: &AggregatedUsage) -> [(&'static str, String); 5] {
    [
        ("event_name", usage.meter.clone()),
        ("payload[stripe_customer_id]", usage.customer_id.clone()),
        ("payload[value]", usage.units.to_string()),
        ("identifier", usage.identifier.clone()),
        ("timestamp", usage.timestamp.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_stripe_meter_event_fields_and_redacts_the_secret() {
        let exporter = StripeExporter::new("sk_test_do_not_log");
        let event = AggregatedUsage {
            identifier: "mcp_usage_fixed".to_owned(),
            customer_id: "cus_acme".to_owned(),
            meter: "mcp_units".to_owned(),
            units: 42,
            timestamp: 1_800_000_000,
        };
        assert_eq!(
            form_fields(&event),
            [
                ("event_name", "mcp_units".to_owned()),
                ("payload[stripe_customer_id]", "cus_acme".to_owned()),
                ("payload[value]", "42".to_owned()),
                ("identifier", "mcp_usage_fixed".to_owned()),
                ("timestamp", "1800000000".to_owned()),
            ]
        );
        assert!(!format!("{exporter:?}").contains("sk_test_do_not_log"));
        assert!(!format!("{exporter:?}").contains(DEFAULT_ENDPOINT));
    }

    #[test]
    fn endpoint_override_cannot_exfiltrate_the_secret() {
        assert!(endpoint_is_allowed(DEFAULT_ENDPOINT));
        assert!(endpoint_is_allowed(
            "http://127.0.0.1:8080/v1/billing/meter_events"
        ));
        assert!(!endpoint_is_allowed(
            "https://attacker.example/v1/billing/meter_events"
        ));
        assert!(!endpoint_is_allowed(
            "http://api.stripe.com/v1/billing/meter_events"
        ));
    }
}
