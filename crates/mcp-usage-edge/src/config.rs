//! Sidecar configuration, parsed from TOML.
//!
//! The sidecar is deployed by operators who are not necessarily Rust
//! programmers, so every knob the embedded library exposes through a builder
//! method is reachable here as a named field with the same default. Anything
//! that changes what gets billed is explicit in the file rather than implied.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use mcp_usage_kit::{PriceBook, Tenant, WeakApiKey, validate_api_key_strength};
use serde::Deserialize;

/// Why a configuration file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// Path that was attempted.
        path: String,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or does not match the schema.
    #[error("cannot parse {path}: {source}")]
    Parse {
        /// Path that was attempted.
        path: String,
        /// Underlying deserialization failure.
        source: toml::de::Error,
    },
    /// `upstream.url` is not an absolute URL with a host.
    #[error("upstream.url must be an absolute http(s) URL with a host, got {0:?}")]
    UpstreamUrl(String),
    /// A tenant declared neither `api_key` nor `api_key_env`.
    #[error("tenant {0:?} must set exactly one of api_key or api_key_env")]
    TenantKeySource(String),
    /// `api_key_env` named a variable that is unset or empty.
    #[error("tenant {tenant:?} reads its key from ${var}, which is unset or empty")]
    TenantKeyEnvMissing {
        /// Tenant identifier.
        tenant: String,
        /// Environment variable that was consulted.
        var: String,
    },
    /// A configured key is too weak to be looked up by digest.
    #[error("tenant {tenant:?} has a weak API key: {source}")]
    TenantKeyWeak {
        /// Tenant identifier.
        tenant: String,
        /// Which strength rule failed.
        source: WeakApiKey,
    },
    /// Two tenants presented the same key.
    #[error("API key for tenant {0:?} is already registered to another tenant")]
    DuplicateTenantKey(String),
    /// No tenant was configured, so nothing could ever authenticate.
    #[error("no tenants configured; the sidecar would refuse every request")]
    NoTenants,
    /// `edge.auth_failure_limit.window_seconds` was zero.
    #[error("edge.auth_failure_limit.window_seconds must be greater than zero")]
    ZeroAuthFailureWindow,
    /// Both tenant sources were configured, or neither.
    #[error("configure exactly one tenant source: a [[tenants]] table or [control_plane]")]
    AmbiguousTenantSource,
    /// `control_plane.token_env` named a variable that is unset or empty.
    #[error("control plane token reads from ${0}, which is unset or empty")]
    PlaneTokenMissing(String),
    /// `control_plane.url` is not absolute.
    #[error("control_plane.url must be an absolute http(s) URL, got {0:?}")]
    PlaneUrl(String),
    /// The control-plane exporter was selected without a control plane.
    #[error("exporter.kind = \"control-plane\" requires a [control_plane] section")]
    ExporterWithoutPlane,
    /// A staleness budget that does not exceed the refresh interval.
    #[error("control_plane.max_stale_seconds must exceed refresh_interval_seconds")]
    StaleBudgetTooSmall,
    /// An MPP secret or token variable is unset or empty.
    #[error("MPP reads a secret from ${0}, which is unset or empty")]
    MppSecretMissing(String),
    /// `[mpp]` was configured without a control plane.
    #[error(
        "[mpp] requires [control_plane]: a challenge is offered when a tenant is over quota, \
         and quota needs the authoritative counters only a control plane has"
    )]
    MppWithoutPlane,
    /// `[mpp]` was configured with quota enforcement turned off.
    #[error(
        "[mpp] requires control_plane.enforce_quota: a challenge is only ever offered when \
         quota refuses a call, so with enforcement off the section would do nothing"
    )]
    MppWithoutQuota,
    /// `mpp.facilitator.url` is not usable.
    #[error("mpp.facilitator.url must be an absolute https URL, got {0:?}")]
    FacilitatorUrl(String),
    /// A zero-length challenge lifetime.
    #[error("mpp.challenge_ttl_seconds must be greater than zero")]
    ZeroChallengeTtl,
}

/// A parsed sidecar configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the sidecar listens on.
    pub listen: SocketAddr,
    /// Where metered traffic is forwarded.
    pub upstream: Upstream,
    /// Metering edge behaviour.
    #[serde(default)]
    pub edge: EdgeSettings,
    /// Where usage aggregates are delivered.
    #[serde(default)]
    pub exporter: ExporterSettings,
    /// Static tenant table. Mutually exclusive with `control_plane`.
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
    /// Hosted control plane. Mutually exclusive with `tenants`.
    #[serde(default)]
    pub control_plane: Option<ControlPlaneSettings>,
    /// Accept MPP payment instead of refusing an over-quota call.
    #[serde(default)]
    pub mpp: Option<MppSettings>,
}

/// The "Payment" HTTP authentication scheme, as this sidecar offers it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MppSettings {
    /// Protection space advertised in the challenge.
    pub realm: String,
    /// Lowercase payment method identifier, such as `tempo` or `usdc`.
    pub method: String,
    /// Registered intent. `charge` is a one-time payment that settles now.
    #[serde(default = "default_intent")]
    pub intent: String,
    /// ISO 4217 code, lowercase.
    pub currency: String,
    /// Who is paid. Interpreted by the payment method.
    pub recipient: String,
    /// Environment variable holding the challenge-binding secret.
    pub secret_env: String,
    /// How long a challenge stays answerable.
    #[serde(default = "default_challenge_ttl_seconds")]
    pub challenge_ttl_seconds: u64,
    /// How many spent proofs to remember.
    #[serde(default = "default_replay_capacity")]
    pub replay_capacity: usize,
    /// Where proofs are verified.
    pub facilitator: FacilitatorSettings,
}

/// The service that answers whether a proof settled.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacilitatorSettings {
    /// Absolute URL of the verification endpoint.
    pub url: String,
    /// Environment variable holding a bearer token for it, when it needs one.
    #[serde(default)]
    pub token_env: Option<String>,
    /// How long to wait for a verdict.
    #[serde(default = "default_facilitator_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_intent() -> String {
    "charge".to_owned()
}

const fn default_challenge_ttl_seconds() -> u64 {
    300
}

const fn default_replay_capacity() -> usize {
    100_000
}

const fn default_facilitator_timeout_seconds() -> u64 {
    20
}

impl MppSettings {
    /// How long a challenge stays answerable.
    #[must_use]
    pub const fn challenge_ttl(&self) -> Duration {
        Duration::from_secs(self.challenge_ttl_seconds)
    }

    /// Resolve the challenge-binding secret.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::MppSecretMissing`] when the variable is unset or
    /// empty. There is no default: an unbound or predictably bound challenge
    /// lets an agent mint its own and set its own price.
    pub fn secret(&self) -> Result<Vec<u8>, ConfigError> {
        std::env::var(&self.secret_env)
            .ok()
            .filter(|value| !value.is_empty())
            .map(String::into_bytes)
            .ok_or_else(|| ConfigError::MppSecretMissing(self.secret_env.clone()))
    }
}

impl FacilitatorSettings {
    /// How long to wait for a verdict.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds)
    }

    /// Resolve the facilitator bearer token, when one is configured.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::MppSecretMissing`] when a variable is named but
    /// unset, which is a configuration mistake rather than "no token".
    pub fn token(&self) -> Result<Option<String>, ConfigError> {
        let Some(name) = &self.token_env else {
            return Ok(None);
        };
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .map(Some)
            .ok_or_else(|| ConfigError::MppSecretMissing(name.clone()))
    }
}

/// The MCP server this sidecar fronts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Absolute base URL, for example `http://127.0.0.1:3000`.
    pub url: String,
    /// How long to wait for upstream headers.
    ///
    /// Generous by default because a task-backed tool call legitimately takes
    /// minutes, and a sidecar that times out early would turn a billable
    /// completion into a free error.
    #[serde(default = "default_upstream_timeout_seconds")]
    pub timeout_seconds: u64,
}

const fn default_upstream_timeout_seconds() -> u64 {
    300
}

impl Upstream {
    /// Header-read timeout as a `Duration`.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds)
    }
}

/// Knobs mirroring `EdgeConfig`'s builder.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSettings {
    /// Refuse protocol revisions older than the one the meter understands.
    ///
    /// Defaults to `false`, matching the library. A sidecar whose whole promise
    /// is "change nothing" must not start returning 400 to the older clients
    /// that are still the majority; classifying them from their body costs one
    /// bounded JSON parse and keeps them working.
    #[serde(default)]
    pub strict_protocol_version: bool,
    /// Forward the caller's credential to the upstream server.
    ///
    /// Defaults to `true` here, unlike the library, because in a sidecar the
    /// upstream is someone else's already-authenticated MCP server. Turning
    /// this off silently breaks their auth.
    #[serde(default = "default_true")]
    pub credential_forwarding: bool,
    /// Largest request body the meter will buffer.
    #[serde(default)]
    pub max_request_body_bytes: Option<usize>,
    /// Largest response prefix the meter will capture when classifying.
    #[serde(default)]
    pub max_response_capture_bytes: Option<usize>,
    /// Meter name reported on exported aggregates.
    #[serde(default)]
    pub meter_name: Option<String>,
    /// Authorization-aware response cache.
    #[serde(default)]
    pub cache: Option<CacheSettings>,
    /// Ceiling on sustained credential guessing across the whole edge.
    ///
    /// Worth setting on an internet-facing sidecar. It is not per-client
    /// limiting, which needs a client identity the meter cannot trust.
    #[serde(default)]
    pub auth_failure_limit: Option<AuthFailureLimitSettings>,
}

const fn default_true() -> bool {
    true
}

impl Default for EdgeSettings {
    fn default() -> Self {
        Self {
            strict_protocol_version: false,
            credential_forwarding: true,
            max_request_body_bytes: None,
            max_response_capture_bytes: None,
            meter_name: None,
            cache: None,
            auth_failure_limit: None,
        }
    }
}

/// Bound on failed credential attempts per window.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthFailureLimitSettings {
    /// Failures tolerated inside one window.
    pub max_failures: u64,
    /// Length of the window. Must be non-zero.
    pub window_seconds: u64,
}

impl AuthFailureLimitSettings {
    /// Window length as a `Duration`.
    #[must_use]
    pub const fn window(&self) -> Duration {
        Duration::from_secs(self.window_seconds)
    }
}

/// Response cache sizing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSettings {
    /// Maximum cached entries.
    pub max_entries: usize,
    /// Maximum time an entry is served for.
    pub max_ttl_seconds: u64,
    /// Whether entries without an authorization context may be shared.
    #[serde(default)]
    pub public_sharing: bool,
}

impl CacheSettings {
    /// Maximum entry lifetime as a `Duration`.
    #[must_use]
    pub const fn max_ttl(&self) -> Duration {
        Duration::from_secs(self.max_ttl_seconds)
    }
}

/// Where aggregates go once the meter has flushed them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExporterSettings {
    /// Which exporter receives flushed batches.
    #[serde(default)]
    pub kind: ExporterKind,
    /// How often the background task drains the buffer.
    ///
    /// The buffer aggregates, so a longer interval means fewer, larger exports
    /// rather than lost usage. It bounds how much usage is in memory and would
    /// be lost to a hard kill, so it is a durability knob, not a latency one.
    #[serde(default = "default_flush_interval_seconds")]
    pub flush_interval_seconds: u64,
}

const fn default_flush_interval_seconds() -> u64 {
    10
}

impl Default for ExporterSettings {
    fn default() -> Self {
        Self {
            kind: ExporterKind::default(),
            flush_interval_seconds: default_flush_interval_seconds(),
        }
    }
}

impl ExporterSettings {
    /// Flush cadence as a `Duration`.
    #[must_use]
    pub const fn flush_interval(&self) -> Duration {
        Duration::from_secs(self.flush_interval_seconds)
    }
}

/// Supported exporter backends.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExporterKind {
    /// Structured logs. The default, and the only one that needs no credentials.
    #[default]
    Log,
    /// Post aggregates to the configured control plane.
    ControlPlane,
}

/// Connection to a hosted control plane.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPlaneSettings {
    /// Base URL, for example `https://plane.example.com`.
    pub url: String,
    /// Environment variable holding this edge's token.
    pub token_env: String,
    /// How often to pull a fresh snapshot.
    #[serde(default = "default_refresh_interval_seconds")]
    pub refresh_interval_seconds: u64,
    /// How long a snapshot may keep serving once refreshes start failing.
    ///
    /// Past this the edge fails closed, because serving hours-old revocations
    /// is worse than refusing. Keep it well above the refresh interval so an
    /// ordinary blip never reaches it.
    #[serde(default = "default_max_stale_seconds")]
    pub max_stale_seconds: u64,
    /// Per-request timeout on calls to the plane.
    #[serde(default = "default_plane_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Refuse calls from tenants already over quota.
    #[serde(default = "default_true")]
    pub enforce_quota: bool,
}

const fn default_refresh_interval_seconds() -> u64 {
    30
}

const fn default_max_stale_seconds() -> u64 {
    900
}

const fn default_plane_timeout_seconds() -> u64 {
    10
}

impl ControlPlaneSettings {
    /// Snapshot refresh cadence.
    #[must_use]
    pub const fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.refresh_interval_seconds)
    }

    /// How long a cached snapshot keeps authenticating.
    #[must_use]
    pub const fn max_stale(&self) -> Duration {
        Duration::from_secs(self.max_stale_seconds)
    }

    /// Per-request timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds)
    }

    /// Resolve the edge token from its environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::PlaneTokenMissing`] when the variable is unset or
    /// empty.
    pub fn token(&self) -> Result<String, ConfigError> {
        std::env::var(&self.token_env)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ConfigError::PlaneTokenMissing(self.token_env.clone()))
    }
}

/// One statically configured tenant.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// Stable internal identifier.
    pub id: String,
    /// Identifier the billing exporter understands.
    pub billing_customer_id: String,
    /// Literal key. Intended for local development only.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable holding the key. Preferred for deployments.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Per-name and per-method pricing.
    #[serde(default)]
    pub prices: PriceBook,
}

impl TenantConfig {
    /// Resolve the key from its configured source and check its strength.
    fn resolve_key(&self) -> Result<String, ConfigError> {
        let key = match (&self.api_key, &self.api_key_env) {
            (Some(key), None) => key.clone(),
            (None, Some(var)) => std::env::var(var)
                .ok()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ConfigError::TenantKeyEnvMissing {
                    tenant: self.id.clone(),
                    var: var.clone(),
                })?,
            _ => return Err(ConfigError::TenantKeySource(self.id.clone())),
        };
        validate_api_key_strength(&key).map_err(|source| ConfigError::TenantKeyWeak {
            tenant: self.id.clone(),
            source,
        })?;
        Ok(key)
    }

    /// The `Tenant` this configuration describes.
    fn tenant(&self) -> Tenant {
        Tenant::new(self.id.clone(), self.billing_customer_id.clone())
            .with_prices(self.prices.clone())
    }
}

impl Config {
    /// Parse a configuration file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read, does not parse, or
    /// describes something the sidecar refuses to run with.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Reject configurations that would fail confusingly at runtime.
    fn validate(&self) -> Result<(), ConfigError> {
        let uri: http::Uri = self
            .upstream
            .url
            .parse()
            .map_err(|_| ConfigError::UpstreamUrl(self.upstream.url.clone()))?;
        if uri.host().is_none() || uri.scheme().is_none() {
            return Err(ConfigError::UpstreamUrl(self.upstream.url.clone()));
        }
        if let Some(limit) = &self.edge.auth_failure_limit
            && limit.window_seconds == 0
        {
            return Err(ConfigError::ZeroAuthFailureWindow);
        }
        match (self.tenants.is_empty(), self.control_plane.is_some()) {
            (true, false) => return Err(ConfigError::NoTenants),
            (false, true) => return Err(ConfigError::AmbiguousTenantSource),
            _ => {}
        }

        if let Some(plane) = &self.control_plane {
            let uri: http::Uri = plane
                .url
                .parse()
                .map_err(|_| ConfigError::PlaneUrl(plane.url.clone()))?;
            if uri.host().is_none() || uri.scheme().is_none() {
                return Err(ConfigError::PlaneUrl(plane.url.clone()));
            }
            // A budget at or below the refresh interval turns one missed poll
            // into a full outage, which is the opposite of what it is for.
            if plane.max_stale_seconds <= plane.refresh_interval_seconds {
                return Err(ConfigError::StaleBudgetTooSmall);
            }
        } else if self.exporter.kind == ExporterKind::ControlPlane {
            return Err(ConfigError::ExporterWithoutPlane);
        }

        if let Some(mpp) = &self.mpp {
            // A challenge is only ever issued when a tenant is refused, and
            // only quota refuses. Without a plane the gate never refuses, so
            // the section would silently do nothing.
            let Some(plane) = &self.control_plane else {
                return Err(ConfigError::MppWithoutPlane);
            };
            // Without enforcement the gate never refuses, so a challenge is
            // never issued and the whole section is dead config. Worse, the
            // only observable effect would be that a request carrying a stale
            // credential takes a slower path to the same answer.
            if !plane.enforce_quota {
                return Err(ConfigError::MppWithoutQuota);
            }
            if mpp.challenge_ttl_seconds == 0 {
                return Err(ConfigError::ZeroChallengeTtl);
            }
            let uri: http::Uri = mpp
                .facilitator
                .url
                .parse()
                .map_err(|_| ConfigError::FacilitatorUrl(mpp.facilitator.url.clone()))?;
            let loopback = uri
                .host()
                .and_then(|host| {
                    host.trim_matches(|c| c == '[' || c == ']')
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|address| address.is_loopback());
            // A facilitator sees payment proofs, so plaintext is only ever
            // acceptable to a loopback test server.
            if uri.host().is_none() || (uri.scheme_str() != Some("https") && !loopback) {
                return Err(ConfigError::FacilitatorUrl(mpp.facilitator.url.clone()));
            }
        }
        Ok(())
    }

    /// Resolve every tenant key, rejecting weak or duplicated keys.
    ///
    /// Returns pairs rather than a populated store so the caller decides which
    /// `TenantStore` implementation receives them.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a key is missing, weak, or shared between two
    /// tenants.
    pub fn resolve_tenants(&self) -> Result<Vec<(String, Tenant)>, ConfigError> {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let mut resolved = Vec::with_capacity(self.tenants.len());
        for entry in &self.tenants {
            let key = entry.resolve_key()?;
            if let Some(existing) = seen.insert(key.clone(), entry.id.clone()) {
                tracing::error!(
                    tenant = %entry.id,
                    conflicts_with = %existing,
                    "duplicate API key"
                );
                return Err(ConfigError::DuplicateTenantKey(entry.id.clone()));
            }
            resolved.push((key, entry.tenant()));
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRONG_KEY: &str = "Zq4vN8xR2tLmK7wP1sB6yH3dF9gJ0cVe";

    fn minimal(extra: &str) -> String {
        format!(
            r#"
listen = "127.0.0.1:8080"

[upstream]
url = "http://127.0.0.1:3000"

[[tenants]]
id = "acme"
billing_customer_id = "cus_acme"
api_key = "{STRONG_KEY}"
{extra}
"#
        )
    }

    fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn a_minimal_file_parses_with_sidecar_defaults() {
        let config = parse(&minimal("")).expect("parses");
        config.validate().expect("valid");

        // Credential forwarding defaults ON in the sidecar. The upstream is
        // someone else's authenticated server; dropping the caller's
        // credential would break it.
        assert!(config.edge.credential_forwarding);
        // Strict mode stays OFF by default: turning it on would 400 every
        // client that predates the mirrored headers.
        assert!(!config.edge.strict_protocol_version);
        assert_eq!(config.upstream.timeout_seconds, 300);
        assert_eq!(config.exporter.kind, ExporterKind::Log);
        assert_eq!(config.exporter.flush_interval_seconds, 10);
    }

    #[test]
    fn a_price_book_round_trips_from_toml() {
        let config = parse(&minimal(
            "\n[tenants.prices]\ndefault_units = 2\n\n[tenants.prices.names]\nexpensive = 50\n",
        ))
        .expect("parses");
        let prices = &config.tenants[0].prices;
        assert_eq!(prices.default_units, 2);
        assert_eq!(prices.names.get("expensive"), Some(&50));
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        // A typo in a billing knob must fail loudly, not bill differently.
        let text = minimal("").replace("[upstream]", "[upstream]\ntimeoutseconds = 5");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn a_relative_upstream_url_is_refused() {
        let text = minimal("").replace("http://127.0.0.1:3000", "/mcp");
        let config = parse(&text).expect("parses");
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UpstreamUrl(_))
        ));
    }

    #[test]
    fn an_empty_tenant_table_is_refused() {
        let text = r#"
listen = "127.0.0.1:8080"

[upstream]
url = "http://127.0.0.1:3000"
"#;
        let config = parse(text).expect("parses");
        assert!(matches!(config.validate(), Err(ConfigError::NoTenants)));
    }

    #[test]
    fn a_zero_auth_failure_window_is_refused() {
        let text = minimal("").replace(
            "[upstream]",
            "[edge.auth_failure_limit]\nmax_failures = 10\nwindow_seconds = 0\n\n[upstream]",
        );
        let config = parse(&text).expect("parses");
        assert!(matches!(
            config.validate(),
            Err(ConfigError::ZeroAuthFailureWindow)
        ));
    }

    #[test]
    fn an_auth_failure_limit_parses() {
        let text = minimal("").replace(
            "[upstream]",
            "[edge.auth_failure_limit]\nmax_failures = 25\nwindow_seconds = 60\n\n[upstream]",
        );
        let config = parse(&text).expect("parses");
        let limit = config.edge.auth_failure_limit.expect("present");
        assert_eq!(limit.max_failures, 25);
        assert_eq!(limit.window(), Duration::from_secs(60));
    }

    #[test]
    fn a_weak_key_is_refused_before_the_listener_opens() {
        let text = minimal("").replace(STRONG_KEY, "short");
        let config = parse(&text).expect("parses");
        assert!(matches!(
            config.resolve_tenants(),
            Err(ConfigError::TenantKeyWeak { .. })
        ));
    }

    #[test]
    fn two_tenants_sharing_a_key_are_refused() {
        let text = format!(
            r#"{}
[[tenants]]
id = "other"
billing_customer_id = "cus_other"
api_key = "{STRONG_KEY}"
"#,
            minimal("")
        );
        let config = parse(&text).expect("parses");
        assert!(matches!(
            config.resolve_tenants(),
            Err(ConfigError::DuplicateTenantKey(_))
        ));
    }

    #[test]
    fn declaring_both_key_sources_is_refused() {
        let text = minimal("api_key_env = \"SOME_VAR\"");
        let config = parse(&text).expect("parses");
        assert!(matches!(
            config.resolve_tenants(),
            Err(ConfigError::TenantKeySource(_))
        ));
    }

    #[test]
    fn resolving_a_valid_table_yields_priced_tenants() {
        let config = parse(&minimal("")).expect("parses");
        let resolved = config.resolve_tenants().expect("resolves");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, STRONG_KEY);
        assert_eq!(resolved[0].1.id, "acme");
        assert_eq!(resolved[0].1.billing_customer_id, "cus_acme");
    }
}
