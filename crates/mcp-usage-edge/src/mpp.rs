//! The "Payment" HTTP authentication scheme, server side.
//!
//! MPP is an *inbound* protocol. An agent calls a resource, receives
//! `402 Payment Required` carrying a `WWW-Authenticate: Payment` challenge,
//! pays out of band, and retries with a credential; the server verifies it,
//! serves the resource, and returns a `Payment-Receipt`. There is nothing to
//! export to and no pre-402 discovery: it is reactive, which is why it belongs
//! at the edge rather than in a control plane.
//!
//! Implemented from `draft-ryan-httpauth-payment-01` (March 2026):
//! <https://datatracker.ietf.org/doc/html/draft-ryan-httpauth-payment-01>,
//! with the problem registry at <https://paymentauth.org/problems/>.
//!
//! ## What is here and what is not
//!
//! The scheme itself is method-agnostic, and so is this: challenge
//! construction and its HMAC binding, credential parsing, body binding,
//! expiry, single-use enforcement and receipt emission are all implemented to
//! the draft. Verifying that a payment actually happened is defined by each
//! payment method's own specification against its own network, so that is a
//! [`PaymentMethod`] the deployment supplies.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::digest::KeyInit as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The credential header used when the challenge carries `header`.
///
/// The draft allows a server to move the credential off `Authorization`, and
/// this sidecar always does: `Authorization` already carries the tenant's own
/// bearer key on its way to the upstream, and overwriting it would break the
/// customer's authentication to fund a payment.
pub const PAYMENT_AUTHORIZATION: &str = "Payment-Authorization";

/// Base URI of the problem registry.
const PROBLEM_BASE: &str = "https://paymentauth.org/problems/";

/// Why a presented credential was refused.
///
/// Each maps to a registered problem type, so an agent can branch on the URI
/// rather than parse prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentError {
    /// No credential was presented.
    Required,
    /// The credential was not base64url, not JSON, or missing fields.
    Malformed,
    /// The challenge binding did not verify: it was not issued by this server,
    /// or its parameters were altered after issue.
    InvalidChallenge,
    /// The challenge's `expires` has passed.
    Expired,
    /// The body digest did not match the one the challenge was bound to.
    DigestMismatch,
    /// The payment method is not one this server accepts.
    MethodUnsupported,
    /// The proof did not verify, or has already been spent.
    VerificationFailed,
    /// The proof was for less than the challenge asked.
    Insufficient,
}

impl PaymentError {
    /// The registered problem type URI.
    #[must_use]
    pub fn problem_type(self) -> String {
        let code = match self {
            Self::Required => "payment-required",
            // A digest mismatch means the credential does not belong to this
            // request, which is a malformed presentation rather than a failed
            // payment; the registry has no separate code for it.
            Self::Malformed | Self::DigestMismatch => "malformed-credential",
            Self::InvalidChallenge => "invalid-challenge",
            Self::Expired => "payment-expired",
            Self::MethodUnsupported => "method-unsupported",
            Self::VerificationFailed => "verification-failed",
            Self::Insufficient => "payment-insufficient",
        };
        format!("{PROBLEM_BASE}{code}")
    }

    /// Short human-readable title. Display only; never relied on by a client.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Required => "Payment required",
            Self::Malformed => "Malformed payment credential",
            Self::InvalidChallenge => "Invalid challenge",
            Self::Expired => "Payment challenge expired",
            Self::DigestMismatch => "Request body does not match the challenge",
            Self::MethodUnsupported => "Unsupported payment method",
            Self::VerificationFailed => "Payment verification failed",
            Self::Insufficient => "Payment insufficient",
        }
    }
}

/// What a payment is being asked for. Base64url-encoded into `request`.
///
/// Amounts are minor-unit strings rather than numbers, matching the draft's own
/// example, which keeps them exact across JSON implementations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaymentRequest {
    /// Amount in the currency's minor unit, as a decimal string.
    pub amount: String,
    /// ISO 4217 code, lowercase.
    pub currency: String,
    /// Who is paid. Interpreted by the payment method.
    pub recipient: String,
}

/// Serialize to canonical JSON, then base64url without padding.
///
/// The draft requires JCS (RFC 8785) before encoding. For an object of string
/// values this reduces to sorted keys and no insignificant whitespace, which is
/// exactly what a `BTreeMap` through `serde_json` produces. A method whose
/// request carries numbers or nested objects needs a full JCS implementation
/// rather than this.
fn encode_jcs(request: &PaymentRequest) -> String {
    let canonical: BTreeMap<&str, &str> = BTreeMap::from([
        ("amount", request.amount.as_str()),
        ("currency", request.currency.as_str()),
        ("recipient", request.recipient.as_str()),
    ]);
    let json = serde_json::to_vec(&canonical).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(json)
}

/// An RFC 9530 `Content-Digest` value over a request body: `sha-256=:<b64>:`.
#[must_use]
pub fn content_digest(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    format!(
        "sha-256=:{}:",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// The challenge parameters a server binds and a client echoes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    /// HMAC binding over every other field.
    pub id: String,
    /// Protection space.
    pub realm: String,
    /// Lowercase payment method identifier.
    pub method: String,
    /// Registered intent. `charge` for a one-time, immediately settled payment.
    pub intent: String,
    /// Base64url JCS of the [`PaymentRequest`].
    pub request: String,
    /// RFC 3339 expiry.
    pub expires: Option<String>,
    /// RFC 9530 digest of the body this challenge is bound to.
    pub digest: Option<String>,
    /// Display-only text.
    pub description: Option<String>,
    /// Opaque state echoed back unchanged.
    pub opaque: Option<String>,
    /// When set, the credential arrives in that header instead of `Authorization`.
    pub header: Option<String>,
}

/// Everything a challenge `id` is bound over.
///
/// A struct rather than a long argument list because the order is load-bearing:
/// these are positional slots in the signed input, and swapping two of them
/// silently changes what a signature means.
#[derive(Debug, Clone, Copy)]
pub struct Binding<'a> {
    /// Protection space.
    pub realm: &'a str,
    /// Payment method identifier.
    pub method: &'a str,
    /// Payment intent.
    pub intent: &'a str,
    /// Base64url JCS of the payment request.
    pub request: &'a str,
    /// RFC 3339 expiry, when present.
    pub expires: Option<&'a str>,
    /// RFC 9530 body digest, when present.
    pub digest: Option<&'a str>,
    /// Opaque state, when present.
    pub opaque: Option<&'a str>,
    /// Credential header override, when present.
    pub header: Option<&'a str>,
}

/// Compute the challenge `id`.
///
/// The draft's recommended stateless binding: HMAC-SHA256 over fixed positional
/// slots joined with `|`, with absent optional fields contributing an empty
/// slot rather than being omitted. Positional slots are what stop two different
/// challenges from colliding on the same input string.
///
/// # Panics
///
/// Does not panic in practice: HMAC accepts a key of any length, so the only
/// fallible step here cannot fail.
#[must_use]
pub fn challenge_id(secret: &[u8], binding: &Binding<'_>) -> String {
    let mut input = [
        binding.realm,
        binding.method,
        binding.intent,
        binding.request,
        binding.expires.unwrap_or(""),
        binding.digest.unwrap_or(""),
        binding.opaque.unwrap_or(""),
    ]
    .join("|");
    if let Some(header) = binding.header {
        input.push('|');
        input.push_str(header);
    }

    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(input.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Quote and escape an auth-param value per RFC 9110 `quoted-string`.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        if character == '"' || character == '\\' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

/// What a challenge is being issued for.
#[derive(Debug, Clone)]
pub struct ChallengeSpec<'a> {
    /// Protection space.
    pub realm: &'a str,
    /// Payment method identifier.
    pub method: &'a str,
    /// Payment intent.
    pub intent: &'a str,
    /// What is being asked for.
    pub request: &'a PaymentRequest,
    /// RFC 3339 expiry.
    pub expires: Option<String>,
    /// RFC 9530 digest of the body this challenge answers.
    pub digest: Option<String>,
    /// Display-only text.
    pub description: Option<String>,
}

impl Challenge {
    /// Build and bind a challenge.
    #[must_use]
    pub fn new(secret: &[u8], spec: ChallengeSpec<'_>) -> Self {
        let encoded = encode_jcs(spec.request);
        // Always moved off `Authorization`: see PAYMENT_AUTHORIZATION.
        let header = Some(PAYMENT_AUTHORIZATION.to_owned());
        let id = challenge_id(
            secret,
            &Binding {
                realm: spec.realm,
                method: spec.method,
                intent: spec.intent,
                request: &encoded,
                expires: spec.expires.as_deref(),
                digest: spec.digest.as_deref(),
                opaque: None,
                header: header.as_deref(),
            },
        );
        Self {
            id,
            realm: spec.realm.to_owned(),
            method: spec.method.to_owned(),
            intent: spec.intent.to_owned(),
            request: encoded,
            expires: spec.expires,
            digest: spec.digest,
            description: spec.description,
            opaque: None,
            header,
        }
    }

    /// The `WWW-Authenticate` value.
    #[must_use]
    pub fn header_value(&self) -> String {
        let mut params = vec![
            format!("id={}", quoted(&self.id)),
            format!("realm={}", quoted(&self.realm)),
            format!("method={}", quoted(&self.method)),
            format!("intent={}", quoted(&self.intent)),
            format!("request={}", quoted(&self.request)),
        ];
        for (name, value) in [
            ("expires", self.expires.as_deref()),
            ("digest", self.digest.as_deref()),
            ("description", self.description.as_deref()),
            ("opaque", self.opaque.as_deref()),
            ("header", self.header.as_deref()),
        ] {
            if let Some(value) = value {
                params.push(format!("{name}={}", quoted(value)));
            }
        }
        format!("Payment {}", params.join(", "))
    }

    /// Re-derive this challenge's binding and compare it to the presented `id`.
    #[must_use]
    pub fn binding_is_valid(&self, secret: &[u8]) -> bool {
        let expected = challenge_id(
            secret,
            &Binding {
                realm: &self.realm,
                method: &self.method,
                intent: &self.intent,
                request: &self.request,
                expires: self.expires.as_deref(),
                digest: self.digest.as_deref(),
                opaque: self.opaque.as_deref(),
                header: self.header.as_deref(),
            },
        );
        constant_time_eq(expected.as_bytes(), self.id.as_bytes())
    }

    /// The decoded payment request, when it is the shape this server issues.
    #[must_use]
    pub fn payment_request(&self) -> Option<PaymentRequest> {
        let raw = URL_SAFE_NO_PAD.decode(&self.request).ok()?;
        serde_json::from_slice(&raw).ok()
    }
}

/// Compare without leaking how much of the value matched.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |differences, (a, b)| differences | (a ^ b))
        == 0
}

/// The challenge as echoed inside a credential.
#[derive(Debug, Clone, Deserialize)]
struct ChallengeEcho {
    id: String,
    realm: String,
    method: String,
    intent: String,
    request: String,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    opaque: Option<String>,
    #[serde(default)]
    header: Option<String>,
}

/// A presented `Payment` credential.
#[derive(Debug, Clone)]
pub struct Credential {
    /// The challenge it answers, as the client echoed it.
    pub challenge: Challenge,
    /// Payer identifier. The draft recommends a DID.
    pub source: Option<String>,
    /// Method-specific proof of payment.
    pub payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CredentialWire {
    challenge: ChallengeEcho,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

/// Parse an `Authorization`/`Payment-Authorization` value.
///
/// # Errors
///
/// Returns [`PaymentError::Malformed`] unless the value is the `Payment`
/// scheme followed by base64url-encoded JSON of the credential shape.
pub fn parse_credential(header_value: &str) -> Result<Credential, PaymentError> {
    let (scheme, encoded) = header_value
        .trim()
        .split_once(' ')
        .ok_or(PaymentError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("Payment") {
        return Err(PaymentError::Malformed);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .map_err(|_| PaymentError::Malformed)?;
    let wire: CredentialWire = serde_json::from_slice(&raw).map_err(|_| PaymentError::Malformed)?;

    Ok(Credential {
        challenge: Challenge {
            id: wire.challenge.id,
            realm: wire.challenge.realm,
            method: wire.challenge.method,
            intent: wire.challenge.intent,
            request: wire.challenge.request,
            expires: wire.challenge.expires,
            digest: wire.challenge.digest,
            description: wire.challenge.description,
            opaque: wire.challenge.opaque,
            header: wire.challenge.header,
        },
        source: wire.source,
        payload: wire.payload,
    })
}

/// A settled payment, returned on a successful response.
#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    /// Always `success`: the draft forbids a receipt on an error response.
    pub status: &'static str,
    /// The method that settled it.
    pub method: String,
    /// RFC 3339 settlement time.
    pub timestamp: String,
    /// Method-specific reference, such as a transaction hash or invoice id.
    pub reference: String,
}

impl Receipt {
    /// Build a receipt for a settled payment.
    #[must_use]
    pub fn new(method: String, reference: String, timestamp: String) -> Self {
        Self {
            status: "success",
            method,
            timestamp,
            reference,
        }
    }

    /// The `Payment-Receipt` value: base64url of the JSON object.
    #[must_use]
    pub fn header_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }
}

/// Boxed future returned by a [`PaymentMethod`].
pub type VerifyFuture<'a> = Pin<Box<dyn Future<Output = Result<String, PaymentError>> + Send + 'a>>;

/// Verifies that a payment actually happened.
///
/// Each payment method's own specification defines this against its own
/// network, so the deployment supplies it. Everything the scheme itself
/// specifies has already been checked before this is called: the binding, the
/// expiry, the body digest and single-use.
pub trait PaymentMethod: Send + Sync {
    /// The lowercase method identifier this verifies, as it appears in a challenge.
    fn method(&self) -> &str;

    /// Verify `payload` settles `request`, returning a settlement reference.
    ///
    /// # Errors
    ///
    /// Returns [`PaymentError::VerificationFailed`] when the proof does not
    /// verify, or [`PaymentError::Insufficient`] when it settles less than was
    /// asked for.
    fn verify<'a>(
        &'a self,
        credential: &'a Credential,
        request: &'a PaymentRequest,
    ) -> VerifyFuture<'a>;
}

// ------------------------------------------------------------- facilitator

/// Verifies a proof by asking a facilitator service.
///
/// The core scheme leaves verification to each payment method's own
/// specification, against its own network. Running that natively means
/// implementing a method spec (Tempo, USDC, Stripe, card) and talking to its
/// settlement layer, which a sidecar has no business doing.
///
/// A facilitator is the pattern deployments actually use: one service holds the
/// network credentials and answers "did this settle, and for how much". **The
/// request and response shape below is this sidecar's own contract, not part of
/// the MPP specification**, which does not define one. It is deliberately small
/// so that fronting an existing facilitator means a thin adapter rather than a
/// rewrite.
///
/// ```json
/// POST <endpoint>
/// {"method":"tempo","intent":"charge","challenge_id":"...","source":"did:...",
///  "request":{"amount":"1000","currency":"usd","recipient":"acct_123"},
///  "payload":{ ...method-specific proof... }}
///
/// 200 {"settled":true,"reference":"0xabc..."}
/// 200 {"settled":false,"reason":"insufficient"}
/// ```
pub struct FacilitatorMethod {
    client: crate::proxy::HttpsClient,
    method: String,
    endpoint: String,
    token: Option<String>,
    timeout: Duration,
}

impl std::fmt::Debug for FacilitatorMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FacilitatorMethod")
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
struct FacilitatorVerdict {
    settled: bool,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

impl FacilitatorMethod {
    /// Point at a facilitator for one payment method.
    #[must_use]
    pub fn new(
        client: crate::proxy::HttpsClient,
        method: String,
        endpoint: String,
        token: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client,
            method,
            endpoint,
            token,
            timeout,
        }
    }
}

impl PaymentMethod for FacilitatorMethod {
    fn method(&self) -> &str {
        &self.method
    }

    fn verify<'a>(
        &'a self,
        credential: &'a Credential,
        request: &'a PaymentRequest,
    ) -> VerifyFuture<'a> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "method": self.method,
                "intent": credential.challenge.intent,
                "challenge_id": credential.challenge.id,
                "source": credential.source,
                "request": request,
                "payload": credential.payload,
            });
            let body = serde_json::to_vec(&payload).map_err(|_| PaymentError::Malformed)?;

            let mut builder = http::Request::builder()
                .method(http::Method::POST)
                .uri(&self.endpoint)
                .header(http::header::CONTENT_TYPE, "application/json")
                .header(http::header::ACCEPT, "application/json");
            if let Some(token) = &self.token {
                builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let request = builder
                .body(http_body_util::Full::new(bytes::Bytes::from(body)))
                .map_err(|_| PaymentError::VerificationFailed)?;

            // A facilitator that does not answer means the payment is unproven,
            // which must never be read as settled.
            let response = tokio::time::timeout(self.timeout, self.client.request(request))
                .await
                .map_err(|_| {
                    tracing::warn!("facilitator timed out; treating the payment as unverified");
                    PaymentError::VerificationFailed
                })?
                .map_err(|_| {
                    tracing::warn!("facilitator unreachable; treating the payment as unverified");
                    PaymentError::VerificationFailed
                })?;

            if !response.status().is_success() {
                tracing::warn!(
                    status = response.status().as_u16(),
                    "facilitator refused to verify"
                );
                return Err(PaymentError::VerificationFailed);
            }

            let bytes = http_body_util::BodyExt::collect(response.into_body())
                .await
                .map_err(|_| PaymentError::VerificationFailed)?
                .to_bytes();
            let verdict: FacilitatorVerdict =
                serde_json::from_slice(&bytes).map_err(|_| PaymentError::VerificationFailed)?;

            if !verdict.settled {
                return Err(match verdict.reason.as_deref() {
                    Some("insufficient") => PaymentError::Insufficient,
                    _ => PaymentError::VerificationFailed,
                });
            }
            // A settlement with no reference cannot be reconciled later, so it
            // is not something to hand back in a receipt.
            verdict.reference.ok_or(PaymentError::VerificationFailed)
        })
    }
}

/// Single-use enforcement for payment proofs.
///
/// The draft requires that a proof be usable exactly once and that concurrent
/// presentations of the same credential settle at most once. Claiming is
/// therefore a test-and-set under one lock rather than a check followed by a
/// later insert.
///
/// This is process-local. A horizontally scaled deployment needs a shared
/// store, the same way durable task attribution does.
#[derive(Debug)]
pub struct ReplayGuard {
    spent: Mutex<HashSet<String>>,
    capacity: usize,
}

impl ReplayGuard {
    /// Track at most `capacity` spent proofs.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            spent: Mutex::new(HashSet::new()),
            capacity,
        }
    }

    /// Claim a proof. `true` the first time, `false` on every later attempt.
    pub fn claim(&self, key: &str) -> bool {
        let mut spent = self
            .spent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bounded so a long-running sidecar cannot be grown without limit by
        // unique credentials. Clearing rather than evicting one entry keeps the
        // structure simple; the window it reopens is bounded by `capacity`, and
        // a deployment that cannot accept that needs the shared store anyway.
        if spent.len() >= self.capacity {
            tracing::warn!(
                capacity = self.capacity,
                "spent-proof set is full; clearing it reopens a replay window"
            );
            spent.clear();
        }
        spent.insert(key.to_owned())
    }

    /// Number of proofs currently remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether nothing has been spent yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything a [`Payments`] needs except its verification method.
#[derive(Debug, Clone)]
pub struct PaymentsConfig {
    /// Secret the challenge binding is computed under.
    pub secret: Vec<u8>,
    /// Protection space advertised in challenges.
    pub realm: String,
    /// Registered intent.
    pub intent: String,
    /// ISO 4217 code, lowercase.
    pub currency: String,
    /// Who is paid.
    pub recipient: String,
    /// How long a challenge stays answerable.
    pub ttl: Duration,
    /// How many spent proofs to remember.
    pub replay_capacity: usize,
}

/// How this sidecar issues and checks challenges.
pub struct Payments {
    secret: Vec<u8>,
    realm: String,
    intent: String,
    currency: String,
    recipient: String,
    ttl: Duration,
    method: Box<dyn PaymentMethod>,
    replay: ReplayGuard,
}

impl std::fmt::Debug for Payments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payments")
            .field("realm", &self.realm)
            .field("method", &self.method.method())
            .field("intent", &self.intent)
            .field("currency", &self.currency)
            .field("secret", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Payments {
    /// Construct from a verified configuration.
    #[must_use]
    pub fn new(config: PaymentsConfig, method: Box<dyn PaymentMethod>) -> Self {
        Self {
            secret: config.secret,
            realm: config.realm,
            intent: config.intent,
            currency: config.currency,
            recipient: config.recipient,
            ttl: config.ttl,
            method,
            replay: ReplayGuard::new(config.replay_capacity),
        }
    }

    /// The method identifier challenges are issued for.
    #[must_use]
    pub fn method(&self) -> &str {
        self.method.method()
    }

    /// Issue a challenge for a call priced at `amount_minor`, bound to `body`.
    #[must_use]
    pub fn challenge(
        &self,
        amount_minor: u64,
        body: &[u8],
        now: &chrono_lite::Rfc3339,
    ) -> Challenge {
        let request = PaymentRequest {
            amount: amount_minor.to_string(),
            currency: self.currency.clone(),
            recipient: self.recipient.clone(),
        };
        Challenge::new(
            &self.secret,
            ChallengeSpec {
                realm: &self.realm,
                method: self.method.method(),
                intent: &self.intent,
                request: &request,
                expires: Some(now.plus(self.ttl)),
                // Bound to this exact body, so a credential cannot be moved to
                // a different, more expensive call.
                digest: Some(content_digest(body)),
                description: None,
            },
        )
    }

    /// Run every check the scheme specifies, then the method's own.
    ///
    /// # Errors
    ///
    /// Returns the [`PaymentError`] whose problem type the caller should
    /// report, having settled nothing.
    pub async fn verify(
        &self,
        header_value: &str,
        body: &[u8],
        now: &chrono_lite::Rfc3339,
    ) -> Result<Receipt, PaymentError> {
        let credential = parse_credential(header_value)?;
        let challenge = &credential.challenge;

        // The binding proves this server issued exactly these parameters. It
        // runs first: everything after it would otherwise be trusting values an
        // agent chose, including the amount.
        if !challenge.binding_is_valid(&self.secret) {
            return Err(PaymentError::InvalidChallenge);
        }
        if challenge.realm != self.realm {
            return Err(PaymentError::InvalidChallenge);
        }
        if challenge.method != self.method.method() {
            return Err(PaymentError::MethodUnsupported);
        }
        if let Some(expires) = challenge.expires.as_deref()
            && now.is_after(expires)
        {
            return Err(PaymentError::Expired);
        }
        if let Some(digest) = challenge.digest.as_deref()
            && !constant_time_eq(digest.as_bytes(), content_digest(body).as_bytes())
        {
            return Err(PaymentError::DigestMismatch);
        }

        let request = challenge
            .payment_request()
            .ok_or(PaymentError::InvalidChallenge)?;

        // Claimed before verification, so two concurrent presentations of one
        // credential cannot both reach the method and settle twice. The cost is
        // that a proof rejected by the method is still burned, which is the
        // safe direction: the agent retries against a fresh challenge.
        if !self.replay.claim(&challenge.id) {
            tracing::warn!("refusing a replayed payment credential");
            return Err(PaymentError::VerificationFailed);
        }

        let reference = self.method.verify(&credential, &request).await?;
        Ok(Receipt::new(
            challenge.method.clone(),
            reference,
            now.to_string(),
        ))
    }
}

/// Just enough RFC 3339 to issue and compare an `expires`.
///
/// A full date-time library is not worth a dependency here: the sidecar only
/// ever produces UTC instants and compares them lexically, which is sound
/// because RFC 3339 UTC timestamps of equal precision sort in time order.
pub mod chrono_lite {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// A UTC instant, rendered as `YYYY-MM-DDTHH:MM:SSZ`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Rfc3339(String, u64);

    impl Rfc3339 {
        /// The current time.
        #[must_use]
        pub fn now() -> Self {
            Self::from_unix(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            )
        }

        /// A specific Unix second.
        #[must_use]
        pub fn from_unix(seconds: u64) -> Self {
            Self(format(seconds), seconds)
        }

        /// This instant plus `offset`, rendered.
        #[must_use]
        pub fn plus(&self, offset: Duration) -> String {
            format(self.1.saturating_add(offset.as_secs()))
        }

        /// Whether this instant is strictly after `timestamp`.
        ///
        /// A timestamp this cannot parse is treated as already past: an expiry
        /// the server cannot read must not be read as "never expires".
        #[must_use]
        pub fn is_after(&self, timestamp: &str) -> bool {
            parse(timestamp).is_none_or(|seconds| self.1 > seconds)
        }
    }

    impl std::fmt::Display for Rfc3339 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    const fn is_leap(year: u64) -> bool {
        (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
    }

    const fn days_in_month(year: u64, month: u64) -> u64 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ if is_leap(year) => 29,
            _ => 28,
        }
    }

    fn format(seconds: u64) -> String {
        let (mut days, rest) = (seconds / 86_400, seconds % 86_400);
        let (hour, minute, second) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
        let mut year = 1970;
        loop {
            let length = if is_leap(year) { 366 } else { 365 };
            if days < length {
                break;
            }
            days -= length;
            year += 1;
        }
        let mut month = 1;
        while days >= days_in_month(year, month) {
            days -= days_in_month(year, month);
            month += 1;
        }
        format!(
            "{year:04}-{month:02}-{:02}T{hour:02}:{minute:02}:{second:02}Z",
            days + 1
        )
    }

    fn parse(timestamp: &str) -> Option<u64> {
        let bytes = timestamp.as_bytes();
        if bytes.len() < 20 || !timestamp.ends_with('Z') {
            return None;
        }
        let number = |range: std::ops::Range<usize>| timestamp.get(range)?.parse::<u64>().ok();
        let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
        let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
        if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
            return None;
        }
        if day > days_in_month(year, month) || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        let mut days = 0;
        for y in 1970..year {
            days += if is_leap(y) { 366 } else { 365 };
        }
        for m in 1..month {
            days += days_in_month(year, m);
        }
        days += day - 1;
        Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn timestamps_round_trip_through_the_epoch() {
            for seconds in [0, 1_000_000_000, 1_800_000_000, 4_102_444_800] {
                let rendered = format(seconds);
                assert_eq!(parse(&rendered), Some(seconds), "{rendered}");
            }
        }

        #[test]
        fn known_instants_render_correctly() {
            assert_eq!(format(0), "1970-01-01T00:00:00Z");
            assert_eq!(format(1_000_000_000), "2001-09-09T01:46:40Z");
            // A leap day, which an off-by-one in the month walk would miss.
            assert_eq!(format(1_709_164_800), "2024-02-29T00:00:00Z");
        }

        #[test]
        fn an_unparsable_expiry_is_treated_as_already_past() {
            let now = Rfc3339::from_unix(1_800_000_000);
            // Reading an expiry the server cannot parse as "never expires"
            // would make a malformed challenge immortal.
            for bad in [
                "",
                "not-a-time",
                "2026-13-01T00:00:00Z",
                "2026-02-30T00:00:00Z",
            ] {
                assert!(now.is_after(bad), "{bad:?}");
            }
        }

        #[test]
        fn expiry_comparison_is_ordinary_time_comparison() {
            let now = Rfc3339::from_unix(1_800_000_000);
            assert!(now.is_after(&format(1_799_999_999)));
            assert!(!now.is_after(&format(1_800_000_000)), "equal is not after");
            assert!(!now.is_after(&format(1_800_000_001)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"server-secret-not-a-real-secret";

    fn spec(expires: Option<&str>, digest: Option<String>) -> ChallengeSpec<'static> {
        // Leaked so the spec can borrow it for the duration of a test.
        let request: &'static PaymentRequest = Box::leak(Box::new(request()));
        ChallengeSpec {
            realm: "api.example.com",
            method: "example",
            intent: "charge",
            request,
            expires: expires.map(str::to_owned),
            digest,
            description: None,
        }
    }

    fn request() -> PaymentRequest {
        PaymentRequest {
            amount: "1000".to_owned(),
            currency: "usd".to_owned(),
            recipient: "acct_123".to_owned(),
        }
    }

    #[test]
    fn the_challenge_header_carries_every_required_parameter() {
        let challenge = Challenge::new(SECRET, spec(Some("2026-01-15T12:05:00Z"), None));
        let value = challenge.header_value();
        assert!(value.starts_with("Payment "));
        for required in ["id=", "realm=", "method=", "intent=", "request="] {
            assert!(value.contains(required), "{required} missing from {value}");
        }
        // Always moved off Authorization, which the tenant's bearer key needs.
        assert!(value.contains(r#"header="Payment-Authorization""#));
    }

    #[test]
    fn the_request_parameter_decodes_to_the_payment_request() {
        let challenge = Challenge::new(SECRET, spec(None, None));
        assert_eq!(challenge.payment_request(), Some(request()));
    }

    #[test]
    fn jcs_encoding_sorts_keys_and_omits_whitespace() {
        let encoded = encode_jcs(&request());
        let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(&encoded).unwrap()).unwrap();
        assert_eq!(
            decoded,
            r#"{"amount":"1000","currency":"usd","recipient":"acct_123"}"#
        );
    }

    #[test]
    fn the_binding_verifies_only_against_its_own_parameters() {
        let challenge = Challenge::new(
            SECRET,
            spec(Some("2026-01-15T12:05:00Z"), Some(content_digest(b"body"))),
        );
        assert!(challenge.binding_is_valid(SECRET));
        assert!(!challenge.binding_is_valid(b"another-secret"));

        // Every bound field must break it, or an agent could rewrite it. The
        // amount is the one that costs money.
        for mutate in [
            (|c: &mut Challenge| c.realm = "evil.example".to_owned()) as fn(&mut Challenge),
            |c: &mut Challenge| c.method = "other".to_owned(),
            |c: &mut Challenge| c.intent = "session".to_owned(),
            |c: &mut Challenge| {
                c.request = encode_jcs(&PaymentRequest {
                    amount: "1".to_owned(),
                    currency: "usd".to_owned(),
                    recipient: "acct_123".to_owned(),
                });
            },
            |c: &mut Challenge| c.expires = Some("2099-01-01T00:00:00Z".to_owned()),
            |c: &mut Challenge| c.digest = Some(content_digest(b"other body")),
            |c: &mut Challenge| c.header = None,
        ] {
            let mut tampered = challenge.clone();
            mutate(&mut tampered);
            assert!(
                !tampered.binding_is_valid(SECRET),
                "a tampered challenge still verified"
            );
        }
    }

    #[test]
    fn absent_optional_fields_occupy_their_slot() {
        // Without positional slots, a challenge with expires="a" and no digest
        // would bind identically to one with no expires and digest="a".
        let base = Binding {
            realm: "r",
            method: "m",
            intent: "i",
            request: "req",
            expires: None,
            digest: None,
            opaque: None,
            header: None,
        };
        let collide_one = challenge_id(
            SECRET,
            &Binding {
                expires: Some("a"),
                ..base
            },
        );
        let collide_two = challenge_id(
            SECRET,
            &Binding {
                digest: Some("a"),
                ..base
            },
        );
        assert_ne!(collide_one, collide_two);
    }

    #[test]
    fn a_quoted_parameter_escapes_quotes_and_backslashes() {
        assert_eq!(quoted(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quoted(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn a_credential_round_trips_through_its_header_form() {
        let challenge = Challenge::new(SECRET, spec(None, None));
        let wire = serde_json::json!({
            "challenge": {
                "id": challenge.id, "realm": challenge.realm,
                "method": challenge.method, "intent": challenge.intent,
                "request": challenge.request, "header": challenge.header
            },
            "source": "did:example:payer",
            "payload": {"proof": "0xabc123"}
        });
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wire).unwrap());

        let parsed = parse_credential(&format!("Payment {encoded}")).expect("parses");
        assert_eq!(parsed.challenge, challenge);
        assert_eq!(parsed.source.as_deref(), Some("did:example:payer"));
        assert_eq!(parsed.payload["proof"], "0xabc123");
        // The scheme token is case insensitive per RFC 9110.
        assert!(parse_credential(&format!("payment {encoded}")).is_ok());
    }

    #[test]
    fn a_malformed_credential_is_refused_rather_than_panicking() {
        for value in [
            "",
            "Payment",
            "Bearer abc",
            "Payment !!!not-base64!!!",
            // Valid base64url, but not the credential shape.
            &format!("Payment {}", URL_SAFE_NO_PAD.encode(b"{}")),
        ] {
            assert_eq!(
                parse_credential(value).unwrap_err(),
                PaymentError::Malformed,
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_receipt_is_base64url_json_and_never_claims_anything_but_success() {
        let receipt = Receipt::new(
            "example".to_owned(),
            "0xdeadbeef".to_owned(),
            "2026-01-15T12:00:00Z".to_owned(),
        );
        let decoded: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(receipt.header_value()).unwrap())
                .unwrap();
        assert_eq!(decoded["status"], "success");
        assert_eq!(decoded["method"], "example");
        assert_eq!(decoded["reference"], "0xdeadbeef");
        assert_eq!(decoded["timestamp"], "2026-01-15T12:00:00Z");
    }

    #[test]
    fn a_proof_can_be_claimed_exactly_once() {
        let guard = ReplayGuard::new(16);
        assert!(guard.claim("proof-1"));
        assert!(
            !guard.claim("proof-1"),
            "a spent proof must not be reusable"
        );
        assert!(guard.claim("proof-2"));
        assert_eq!(guard.len(), 2);
    }

    #[test]
    fn the_replay_set_stays_bounded() {
        let guard = ReplayGuard::new(4);
        for index in 0..10 {
            guard.claim(&format!("proof-{index}"));
        }
        assert!(
            guard.len() <= 4,
            "the spent set must not grow without limit"
        );
    }

    #[test]
    fn problem_types_are_the_registered_uris() {
        assert_eq!(
            PaymentError::Required.problem_type(),
            "https://paymentauth.org/problems/payment-required"
        );
        assert_eq!(
            PaymentError::VerificationFailed.problem_type(),
            "https://paymentauth.org/problems/verification-failed"
        );
        assert_eq!(
            PaymentError::Expired.problem_type(),
            "https://paymentauth.org/problems/payment-expired"
        );
        assert_eq!(
            PaymentError::InvalidChallenge.problem_type(),
            "https://paymentauth.org/problems/invalid-challenge"
        );
        assert_eq!(
            PaymentError::MethodUnsupported.problem_type(),
            "https://paymentauth.org/problems/method-unsupported"
        );
        assert_eq!(
            PaymentError::Insufficient.problem_type(),
            "https://paymentauth.org/problems/payment-insufficient"
        );
    }
}
