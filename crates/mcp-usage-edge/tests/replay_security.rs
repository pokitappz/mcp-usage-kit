//! Payment admission must remain single-use across concurrency and process loss.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use mcp_usage_edge::mpp::{
    Challenge, Credential, PaymentError, PaymentMethod, PaymentRequest, Payments, PaymentsConfig,
    VerifyFuture, chrono_lite::Rfc3339,
};
use mcp_usage_edge::replay::{InMemoryReplayStore, ReplayStore};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

const BODY: &[u8] = b"request-body";
const IDENTITY: (&str, &str) = ("tools/call", "sum");

struct Verifier {
    calls: Arc<AtomicUsize>,
    result: Result<String, PaymentError>,
    entered: Option<Arc<tokio::sync::Notify>>,
}
impl PaymentMethod for Verifier {
    fn method(&self) -> &'static str {
        "test"
    }
    fn verify<'a>(&'a self, _: &'a Credential, _: &'a PaymentRequest) -> VerifyFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = &self.entered {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
            self.result.clone()
        })
    }
}

fn payments(store: Arc<dyn ReplayStore>, verifier: Verifier) -> Payments {
    Payments::new(
        PaymentsConfig {
            secret: b"test-signing-secret-with-at-least-32-bytes".to_vec(),
            realm: "test".into(),
            intent: "charge".into(),
            currency: "usd".into(),
            recipient: "merchant".into(),
            ttl: Duration::from_secs(300),
        },
        Box::new(verifier),
        store,
    )
    .unwrap()
}
fn verifier(calls: &Arc<AtomicUsize>, result: Result<String, PaymentError>) -> Verifier {
    Verifier {
        calls: calls.clone(),
        result,
        entered: None,
    }
}
fn credential(challenge: Challenge) -> String {
    let wire = serde_json::json!({"challenge": {
        "id": challenge.id, "realm": challenge.realm, "method": challenge.method,
        "intent": challenge.intent, "request": challenge.request, "expires": challenge.expires,
        "digest": challenge.digest, "opaque": challenge.opaque, "header": challenge.header,
    }, "payload": {"proof": "fixture"}});
    format!(
        "Payment {}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wire).unwrap())
    )
}
fn issue(payments: &Payments) -> String {
    credential(payments.challenge(100, "tenant-a", IDENTITY, BODY, &Rfc3339::from_unix(1000)))
}
async fn verify(payments: &Payments, proof: &str) -> Result<(), PaymentError> {
    payments
        .verify(
            proof,
            "tenant-a",
            BODY,
            IDENTITY,
            100,
            &Rfc3339::from_unix(1000),
        )
        .await
        .map(|_| ())
}

async fn concurrent_instances(
    first_store: Arc<dyn ReplayStore>,
    second_store: Arc<dyn ReplayStore>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let first = payments(first_store, verifier(&calls, Ok("settled".into())));
    let second = payments(second_store.clone(), verifier(&calls, Ok("settled".into())));
    let proof = issue(&first);
    let (a, b) = tokio::join!(verify(&first, &proof), verify(&second, &proof));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(first);
    drop(second);
    let restarted = payments(second_store, verifier(&calls, Ok("settled".into())));
    assert_eq!(
        verify(&restarted, &proof).await,
        Err(PaymentError::VerificationFailed)
    );
}

#[tokio::test]
async fn separate_payment_instances_share_single_use_state() {
    let store = Arc::new(InMemoryReplayStore::default());
    concurrent_instances(store.clone(), store).await;
}

#[tokio::test]
async fn cancellation_and_ambiguous_settlement_do_not_release_reservations() {
    let store = Arc::new(InMemoryReplayStore::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let pending = payments(
        store.clone(),
        Verifier {
            calls: calls.clone(),
            result: Ok("paid".into()),
            entered: Some(entered.clone()),
        },
    );
    let proof = issue(&pending);
    let copy = proof.clone();
    let task = tokio::spawn(async move { verify(&pending, &copy).await });
    entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let restarted = payments(store.clone(), verifier(&calls, Ok("paid".into())));
    assert_eq!(
        verify(&restarted, &proof).await,
        Err(PaymentError::VerificationFailed)
    );
    let ambiguous = payments(store, verifier(&calls, Err(PaymentError::Unavailable)));
    let proof = issue(&ambiguous);
    assert_eq!(
        verify(&ambiguous, &proof).await,
        Err(PaymentError::Unavailable)
    );
    assert_eq!(
        verify(&restarted, &proof).await,
        Err(PaymentError::VerificationFailed)
    );
}

#[tokio::test]
async fn explicit_rejection_releases_but_tenant_and_expiry_mismatches_never_verify() {
    let store = Arc::new(InMemoryReplayStore::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let rejected = payments(
        store.clone(),
        verifier(&calls, Err(PaymentError::NotSettled)),
    );
    let proof = issue(&rejected);
    assert_eq!(
        verify(&rejected, &proof).await,
        Err(PaymentError::NotSettled)
    );
    let accepted = payments(store, verifier(&calls, Ok("settled".into())));
    assert_eq!(
        accepted
            .verify(
                &proof,
                "tenant-b",
                BODY,
                IDENTITY,
                100,
                &Rfc3339::from_unix(1000)
            )
            .await
            .unwrap_err(),
        PaymentError::InvalidChallenge
    );
    assert_eq!(
        accepted
            .verify(
                &proof,
                "tenant-a",
                BODY,
                IDENTITY,
                100,
                &Rfc3339::from_unix(1301)
            )
            .await
            .unwrap_err(),
        PaymentError::Expired
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(verify(&accepted, &proof).await.is_ok());
}

#[tokio::test]
async fn stale_owner_cannot_release_a_new_reservation() {
    let store = InMemoryReplayStore::default();
    let old = store
        .reserve("proof", Duration::from_millis(1))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let current = store
        .reserve("proof", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store.release(&old).await.unwrap();
    assert!(
        store
            .reserve("proof", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
    store.release(&current).await.unwrap();
    assert!(
        store
            .reserve("proof", Duration::from_secs(60))
            .await
            .unwrap()
            .is_some()
    );
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn redis_protection_survives_client_recreation_and_contention() {
    use mcp_usage_edge::replay::RedisReplayStore;
    let Ok(url) = std::env::var("MCP_USAGE_TEST_REDIS_URL") else {
        assert!(
            std::env::var_os("MCP_USAGE_REQUIRE_BACKENDS").is_none(),
            "Redis is required"
        );
        return;
    };
    let prefix = format!("replay-test-{}", uuid::Uuid::new_v4());
    let first = Arc::new(
        RedisReplayStore::connect(&url, &prefix, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    let second = Arc::new(
        RedisReplayStore::connect(&url, &prefix, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    concurrent_instances(first, second).await;
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn replay_store_rejects_remote_plaintext_and_unreachable_endpoints() {
    use mcp_usage_edge::replay::RedisReplayStore;
    assert!(
        RedisReplayStore::connect(
            "redis://example.com:6379",
            "test",
            Duration::from_millis(100)
        )
        .await
        .is_err()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("redis://{}", listener.local_addr().unwrap());
    assert!(
        RedisReplayStore::connect(&url, "test", Duration::from_millis(100))
            .await
            .is_err()
    );
}
