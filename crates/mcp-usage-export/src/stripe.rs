//! Stripe Billing Meter Events exporter.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{AggregatedUsage, BatchExporter, ExportError, ExportFuture};

const DEFAULT_ENDPOINT: &str = "https://api.stripe.com/v1/billing/meter_events";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DEAD_LETTER_CAPACITY: usize = 1_024;
const MAX_EVENT_AGE_SECONDS: u64 = 35 * 24 * 60 * 60;

/// A Stripe aggregate that cannot be submitted without changing its timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeDeadLetter {
    /// The original aggregate, including its stable identifier and timestamp.
    pub aggregate: AggregatedUsage,
    /// Why the aggregate was quarantined.
    pub reason: StripeDeadLetterReason,
}

/// Why a Stripe aggregate was quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeDeadLetterReason {
    /// The timestamp is strictly older than Stripe's 35-day acceptance window.
    TimestampTooOld,
}

#[derive(Default)]
struct StripeState {
    dead_letters: VecDeque<StripeDeadLetter>,
    identifiers: HashSet<String>,
    dropped_dead_letters: u64,
}

/// Exports pre-aggregated usage through Stripe's v1 Billing Meter Events API.
///
/// Every request includes the stable [`AggregatedUsage::identifier`]. Stripe
/// deduplicates meter-event identifiers for at least 24 hours, making a retry
/// safe when the transport fails after Stripe accepted the request.
pub struct StripeExporter {
    client: reqwest::Client,
    secret_key: String,
    endpoint: String,
    dead_letter_capacity: usize,
    state: Mutex<StripeState>,
}

impl StripeExporter {
    /// Construct an exporter targeting Stripe's production API endpoint.
    ///
    /// # Panics
    ///
    /// Panics if the TLS backend cannot be initialized, matching the behavior of
    /// [`reqwest::Client::new`].
    #[must_use]
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self {
            // Redirects are refused so the endpoint allowlist is the last word
            // on where this request goes. Following a 3xx would carry the meter
            // event, including the customer identifier, to a host that was never
            // checked.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client with a no-redirect policy"),
            secret_key: secret_key.into(),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            dead_letter_capacity: DEFAULT_DEAD_LETTER_CAPACITY,
            state: Mutex::new(StripeState::default()),
        }
    }

    /// Override the endpoint for an IP-addressed loopback test server.
    ///
    /// Only `https://api.stripe.com` and loopback IP literals are accepted, so a
    /// configuration error cannot send the Stripe secret to an unrelated server.
    /// The endpoint is checked here rather than at the first flush so a bad value
    /// fails at startup instead of silently deferring until billing runs.
    ///
    /// # Errors
    ///
    /// Returns [`ExportError::Provider`] when the endpoint is not on the
    /// allowlist.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Result<Self, ExportError> {
        let endpoint = endpoint.into();
        if !endpoint_is_allowed(&endpoint) {
            return Err(disallowed_endpoint());
        }
        self.endpoint = endpoint;
        Ok(self)
    }

    /// Bound how many stale aggregates are retained for reconciliation.
    ///
    /// The default is 1,024. Reducing the capacity evicts the oldest retained
    /// entries and increments [`StripeExporter::dropped_dead_letters`]. Zero
    /// disables retention while continuing to count discarded entries.
    #[must_use]
    pub fn with_dead_letter_capacity(mut self, capacity: usize) -> Self {
        self.dead_letter_capacity = capacity;
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.dead_letters.len() > capacity {
            if let Some(evicted) = state.dead_letters.pop_front() {
                state.identifiers.remove(&evicted.aggregate.identifier);
                state.dropped_dead_letters = state.dropped_dead_letters.saturating_add(1);
            }
        }
        self
    }

    /// Number of stale aggregates retained for reconciliation.
    #[must_use]
    pub fn dead_letter_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dead_letters
            .len()
    }

    /// Number of stale aggregates discarded because retention was full.
    #[must_use]
    pub fn dropped_dead_letters(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dropped_dead_letters
    }

    /// Drain the retained stale aggregates for application-owned reconciliation.
    #[must_use]
    pub fn take_dead_letters(&self) -> Vec<StripeDeadLetter> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.identifiers.clear();
        state.dead_letters.drain(..).collect()
    }

    fn quarantine(&self, aggregate: &AggregatedUsage) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.identifiers.contains(&aggregate.identifier) {
            return;
        }
        if self.dead_letter_capacity == 0 {
            state.dropped_dead_letters = state.dropped_dead_letters.saturating_add(1);
            return;
        }
        while state.dead_letters.len() >= self.dead_letter_capacity {
            if let Some(evicted) = state.dead_letters.pop_front() {
                state.identifiers.remove(&evicted.aggregate.identifier);
                state.dropped_dead_letters = state.dropped_dead_letters.saturating_add(1);
            }
        }
        state.identifiers.insert(aggregate.identifier.clone());
        state.dead_letters.push_back(StripeDeadLetter {
            aggregate: aggregate.clone(),
            reason: StripeDeadLetterReason::TimestampTooOld,
        });
    }
}

fn disallowed_endpoint() -> ExportError {
    ExportError::Provider(
        "Stripe endpoint must be api.stripe.com or a loopback test server".to_owned(),
    )
}

impl fmt::Debug for StripeExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StripeExporter")
            .field("secret_key", &"[REDACTED]")
            .field("endpoint", &"[REDACTED]")
            .field("dead_letters", &self.dead_letter_count())
            .field("dropped_dead_letters", &self.dropped_dead_letters())
            .field("dead_letter_capacity", &self.dead_letter_capacity)
            .finish_non_exhaustive()
    }
}

impl BatchExporter for StripeExporter {
    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        Box::pin(async move {
            // `with_endpoint` already refused anything off the allowlist. This
            // re-check costs one URL parse per flush and keeps the guarantee
            // local to the code that actually sends the secret.
            if !endpoint_is_allowed(&self.endpoint) {
                return Err(disallowed_endpoint());
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            for usage in batch
                .iter()
                .filter(|usage| timestamp_too_old(usage.timestamp, now))
            {
                self.quarantine(usage);
            }
            for usage in batch
                .iter()
                .filter(|usage| !timestamp_too_old(usage.timestamp, now))
            {
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

fn timestamp_too_old(timestamp: u64, now: u64) -> bool {
    timestamp < now.saturating_sub(MAX_EVENT_AGE_SECONDS)
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

    fn aggregate(identifier: &str, timestamp: u64) -> AggregatedUsage {
        AggregatedUsage {
            identifier: identifier.to_owned(),
            customer_id: format!("cus_{identifier}"),
            meter: "mcp_units".to_owned(),
            units: 42,
            timestamp,
        }
    }

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

    #[test]
    fn a_disallowed_endpoint_is_refused_at_construction() {
        for endpoint in [
            "https://attacker.example/v1/billing/meter_events",
            "http://api.stripe.com/v1/billing/meter_events",
            // Userinfo does not make the host api.stripe.com.
            "https://api.stripe.com@attacker.example/v1",
            "not a url",
        ] {
            assert!(
                StripeExporter::new("sk_test_do_not_log")
                    .with_endpoint(endpoint)
                    .is_err(),
                "{endpoint} should be refused before any export runs"
            );
        }

        assert!(
            StripeExporter::new("sk_test_do_not_log")
                .with_endpoint("http://127.0.0.1:8080/v1/billing/meter_events")
                .is_ok()
        );
    }

    #[test]
    fn timestamp_cutoff_is_strictly_older_than_35_days() {
        let now = 2_000_000_000;
        let cutoff = now - MAX_EVENT_AGE_SECONDS;
        assert!(!timestamp_too_old(cutoff, now));
        assert!(timestamp_too_old(cutoff - 1, now));
    }

    #[tokio::test]
    async fn stale_aggregates_are_bounded_deduplicated_and_drained_once() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale = now - MAX_EVENT_AGE_SECONDS - 1;
        let exporter = StripeExporter::new("sk_test_do_not_log").with_dead_letter_capacity(1);
        exporter
            .export(&[
                aggregate("old-one", stale),
                aggregate("old-one", stale),
                aggregate("old-two", stale),
            ])
            .await
            .unwrap();

        assert_eq!(exporter.dead_letter_count(), 1);
        assert_eq!(exporter.dropped_dead_letters(), 1);
        let debug = format!("{exporter:?}");
        for private in ["old-one", "old-two", "cus_old-two", "mcp_units"] {
            assert!(!debug.contains(private));
        }
        let dead_letters = exporter.take_dead_letters();
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].aggregate.identifier, "old-two");
        assert_eq!(
            dead_letters[0].reason,
            StripeDeadLetterReason::TimestampTooOld
        );
        assert!(exporter.take_dead_letters().is_empty());
    }

    #[tokio::test]
    async fn mixed_batch_sends_only_fresh_events_without_rewriting_timestamps() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut chunk = [0_u8; 1_024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(position) =
                        request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                while request.len() < header_end + content_length {
                    let mut chunk = [0_u8; 1_024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                requests.push(
                    String::from_utf8(request[header_end..header_end + content_length].to_vec())
                        .unwrap(),
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .unwrap();
            }
            requests
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale = aggregate("stale", now - MAX_EVENT_AGE_SECONDS - 1);
        let fresh_one = aggregate("fresh-one", now - 2);
        let fresh_two = aggregate("fresh-two", now - 1);
        let exporter = StripeExporter::new("sk_test_do_not_log")
            .with_endpoint(format!("http://{address}/v1/billing/meter_events"))
            .unwrap();
        exporter
            .export(&[stale.clone(), fresh_one.clone(), fresh_two.clone()])
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("identifier=fresh-one"));
        assert!(requests[0].contains(&format!("timestamp={}", fresh_one.timestamp)));
        assert!(requests[1].contains("identifier=fresh-two"));
        assert!(requests[1].contains(&format!("timestamp={}", fresh_two.timestamp)));
        assert!(requests.iter().all(|request| !request.contains("stale")));
        assert_eq!(exporter.take_dead_letters()[0].aggregate, stale);
    }
}
