//! Deadlines cover response bodies, including chunked responses without a size hint.
use mcp_usage_edge::control_plane::PlaneClient;
use mcp_usage_edge::mpp::{
    Challenge, Credential, FacilitatorMethod, PaymentMethod, PaymentRequest,
};
use mcp_usage_edge::proxy::build_client;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(recovered_body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        for attempt in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0; 8192];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                match attempt {
                    0 => {
                        // Headers arrive promptly but the body never finishes.
                        socket.write_all(b"1\r\n{\r\n").await.unwrap();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    1 => {
                        // No Content-Length: enforce the cap while collecting.
                        let _ = socket
                            .write_all(
                                format!("100\r\n{}\r\n0\r\n\r\n", "x".repeat(256)).as_bytes(),
                            )
                            .await;
                    }
                    _ => {
                        socket
                            .write_all(
                                format!(
                                    "{:x}\r\n{recovered_body}\r\n0\r\n\r\n",
                                    recovered_body.len()
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                }
            });
        }
    });
    url
}

#[tokio::test]
async fn snapshot_body_is_bounded_and_recovers_after_stall_and_oversize() {
    let url = server(r#"{"tenants":[]}"#).await;
    let client = PlaneClient::new(
        build_client(),
        &url,
        "test".into(),
        Duration::from_millis(150),
    )
    .with_max_response_bytes(64);
    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(1), client.snapshot()).await;
        assert!(
            result
                .expect("body must respect the request deadline")
                .is_err()
        );
    }
    assert!(client.snapshot().await.unwrap().tenants.is_empty());
}

#[tokio::test]
async fn facilitator_body_is_bounded_and_recovers_after_stall_and_oversize() {
    let url = server(r#"{"settled":true,"reference":"paid"}"#).await;
    let method = FacilitatorMethod::new(
        build_client(),
        "example".into(),
        url,
        None,
        Duration::from_millis(150),
    )
    .with_max_response_bytes(64);
    let credential = Credential {
        challenge: Challenge {
            id: "test".into(),
            realm: "test".into(),
            method: "example".into(),
            intent: "charge".into(),
            request: String::new(),
            expires: None,
            digest: None,
            description: None,
            opaque: None,
            header: None,
        },
        source: None,
        payload: serde_json::json!({}),
    };
    let request = PaymentRequest {
        amount: "10".into(),
        currency: "usd".into(),
        recipient: "merchant".into(),
    };
    for _ in 0..2 {
        let result =
            tokio::time::timeout(Duration::from_secs(1), method.verify(&credential, &request))
                .await;
        assert!(
            result
                .expect("body must respect the request deadline")
                .is_err()
        );
    }
    assert_eq!(method.verify(&credential, &request).await.unwrap(), "paid");
}
