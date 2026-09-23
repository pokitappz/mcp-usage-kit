//! Exercise the actual binary's periodic drain and SIGTERM path.
#![cfg(unix)]
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use mcp_usage_edge::proxy::{HttpsClient, build_client};
use mcp_usage_kit::hash_api_key;
use std::collections::HashMap;
use std::convert::Infallible;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const KEY: &str = "Zq4vN8xR2tLmK7wP1sB6yH3dF9gJ0cVe";

struct State {
    fail_exports: AtomicBool,
    attempts: Mutex<Vec<serde_json::Value>>,
    accepted: Mutex<HashMap<String, u64>>,
    slow_started: AtomicUsize,
    release: Semaphore,
}

impl Default for State {
    fn default() -> Self {
        Self {
            fail_exports: AtomicBool::new(false),
            attempts: Mutex::default(),
            accepted: Mutex::default(),
            slow_started: AtomicUsize::new(0),
            release: Semaphore::new(0),
        }
    }
}

async fn origin(state: Arc<State>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: Request<hyper::body::Incoming>| {
                        let state = state.clone();
                        async move {
                            let path = req.uri().path().to_owned();
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let value = serde_json::from_slice::<serde_json::Value>(&body)
                                .unwrap_or_default();
                            let mut status = StatusCode::OK;
                            let answer = match path.as_str() {
                                "/v1/edge/snapshot" => serde_json::json!({"tenants":[{
                                    "api_key_sha256":hash_api_key(KEY), "tenant_id":"tenant", "billing_customer_id":"customer",
                                    "prices":{"default_units":7}, "unit_price_micros":1000
                                }]}),
                                "/v1/edge/usage" => {
                                    state.attempts.lock().unwrap().push(value.clone());
                                    if state.fail_exports.load(Ordering::SeqCst) {
                                        status = StatusCode::SERVICE_UNAVAILABLE;
                                        serde_json::json!({})
                                    } else {
                                        let mut accepted = state.accepted.lock().unwrap();
                                        let outcomes: Vec<_> = value["events"].as_array().unwrap().iter().map(|event| {
                                        accepted.insert(event["identifier"].as_str().unwrap().into(), event["units"].as_u64().unwrap());
                                        serde_json::json!({"identifier":event["identifier"],"outcome":"accepted"})
                                    }).collect();
                                        serde_json::json!({"outcomes":outcomes})
                                    }
                                }
                                _ => {
                                    let name = value["params"]["name"].as_str().unwrap_or_default();
                                    if name == "slow" {
                                        state.slow_started.fetch_add(1, Ordering::SeqCst);
                                        state.release.acquire().await.unwrap().forget();
                                    }
                                    let result = if name == "task" {
                                        serde_json::json!({"resultType":"task","taskId":"job","status":"working"})
                                    } else if value["method"] == "tasks/get" {
                                        serde_json::json!({"resultType":"complete","taskId":"job","status":"completed"})
                                    } else {
                                        serde_json::json!({"resultType":"complete","content":[]})
                                    };
                                    serde_json::json!({"jsonrpc":"2.0","id":1,"result":result})
                                }
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(answer.to_string())))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    url
}

struct Binary {
    child: Child,
    config: std::path::PathBuf,
    url: String,
}
impl Drop for Binary {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config);
    }
}
impl Binary {
    async fn start(origin: &str, extra: &str, shutdown_seconds: u64) -> Self {
        Self::start_with_store(origin, extra, shutdown_seconds, None).await
    }
    async fn start_with_store(
        origin: &str,
        extra: &str,
        shutdown_seconds: u64,
        store: Option<&str>,
    ) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let config =
            std::env::temp_dir().join(format!("mcp-lifecycle-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(
            &config,
            format!(
                r#"
listen = "{port}"
[upstream]
url = "{origin}"
[edge]
shutdown_timeout_seconds = {shutdown_seconds}
[control_plane]
url = "{origin}"
token_env = "LIFECYCLE_TOKEN"
[exporter]
kind = "control-plane"
flush_interval_seconds = 1
{extra}
"#
            ),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_mcp-usage-edge"))
            .arg(&config)
            .env("LIFECYCLE_TOKEN", "test-token")
            .env("LIFECYCLE_REDIS", store.unwrap_or_default())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut binary = Self {
            child,
            config,
            url: format!("http://{port}/mcp"),
        };
        for _ in 0..200 {
            assert!(
                binary.child.try_wait().unwrap().is_none(),
                "binary exited before listening"
            );
            if tokio::net::TcpStream::connect(port).await.is_ok() {
                return binary;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("binary did not listen");
    }
    fn terminate(&self) {
        assert!(
            Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    async fn exited(&mut self) -> bool {
        for _ in 0..240 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("binary did not obey shutdown budget");
    }
}
async fn call(http: HttpsClient, url: String, method: &str, name: &str) {
    let mut request = Request::builder()
        .method("POST")
        .uri(url)
        .header("x-api-key", KEY)
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if method == "tools/call" {
        request = request.header("mcp-name", name);
    }
    let response = http
        .request(
            request
                .body(Full::new(Bytes::from(
                    serde_json::json!({
                        "jsonrpc":"2.0","id":1,"method":method,"params":{"name":name,"taskId":"job"}
                    })
                    .to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();
}
async fn until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition did not become true");
}

#[tokio::test]
async fn sigterm_finishes_active_work_and_exports_retry_and_pending_batches() {
    let state = Arc::new(State::default());
    state.fail_exports.store(true, Ordering::SeqCst);
    let origin = origin(state.clone()).await;
    let mut binary = Binary::start(&origin, "", 5).await;
    let http = build_client();
    call(http.clone(), binary.url.clone(), "tools/call", "first").await;
    until(|| !state.attempts.lock().unwrap().is_empty()).await;
    let active = tokio::spawn(call(http.clone(), binary.url.clone(), "tools/call", "slow"));
    until(|| state.slow_started.load(Ordering::SeqCst) == 1).await;
    binary.terminate();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(binary.child.try_wait().unwrap().is_none());
    state.fail_exports.store(false, Ordering::SeqCst);
    state.release.add_permits(1);
    active.await.unwrap();
    assert!(binary.exited().await);
    assert_eq!(state.accepted.lock().unwrap().values().sum::<u64>(), 14);
    let attempts = state.attempts.lock().unwrap();
    assert_eq!(attempts[0]["events"], attempts[1]["events"]);
    assert_eq!(state.accepted.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn shutdown_budget_bounds_a_connection_that_never_finishes() {
    let state = Arc::new(State::default());
    let origin = origin(state.clone()).await;
    let mut binary = Binary::start(&origin, "", 1).await;
    let active = tokio::spawn(call(
        build_client(),
        binary.url.clone(),
        "tools/call",
        "slow",
    ));
    until(|| state.slow_started.load(Ordering::SeqCst) == 1).await;
    binary.terminate();
    assert!(!binary.exited().await);
    active.abort();
}

/// A minimal RESP2 store whose replies always require an await. This makes the
/// real RedisTaskStore park terminal accounting when the response body drops.
#[cfg(feature = "redis")]
async fn delayed_store() -> String {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("redis://{}", listener.local_addr().unwrap());
    let entries = Arc::new(Mutex::new(HashMap::<Vec<u8>, Vec<u8>>::new()));
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let entries = entries.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read);
                loop {
                    let mut line = String::new();
                    if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let count: usize = line.trim().strip_prefix('*').unwrap().parse().unwrap();
                    let mut args = Vec::new();
                    for _ in 0..count {
                        line.clear();
                        read.read_line(&mut line).await.unwrap();
                        let len: usize = line.trim().strip_prefix('$').unwrap().parse().unwrap();
                        let mut arg = vec![0; len + 2];
                        read.read_exact(&mut arg).await.unwrap();
                        arg.truncate(len);
                        args.push(arg);
                    }
                    let response = {
                        let mut entries = entries.lock().unwrap();
                        match args[0].as_slice() {
                            b"SET" => {
                                entries
                                    .entry(args[1].clone())
                                    .or_insert_with(|| args[2].clone());
                                b"+OK\r\n".to_vec()
                            }
                            b"GET" | b"EVAL" => {
                                let value = if args[0] == b"GET" {
                                    entries.get(&args[1]).cloned()
                                } else {
                                    entries.remove(&args[3])
                                };
                                value.map_or_else(
                                    || b"$-1\r\n".to_vec(),
                                    |value| {
                                        let mut response =
                                            format!("${}\r\n", value.len()).into_bytes();
                                        response.extend(value);
                                        response.extend(b"\r\n");
                                        response
                                    },
                                )
                            }
                            b"DEL" => {
                                entries.remove(&args[1]);
                                b":1\r\n".to_vec()
                            }
                            _ => b"+OK\r\n".to_vec(), // CLIENT SETINFO during connection setup
                        }
                    };
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if write.write_all(&response).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    url
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn idle_periods_and_sigterm_drain_delayed_task_accounting() {
    let store = delayed_store().await;
    // Each run gets a distinct store prefix, while the real binary talks to
    // the delayed RESP server through its production RedisTaskStore.
    for shutdown in [false, true] {
        let state = Arc::new(State::default());
        let origin = origin(state.clone()).await;
        let extra = format!(
            "[task_store]\nurl_env = \"LIFECYCLE_REDIS\"\nkey_prefix = \"test-{}\"\n",
            uuid::Uuid::new_v4()
        );
        let mut binary = Binary::start_with_store(&origin, &extra, 5, Some(&store)).await;
        call(build_client(), binary.url.clone(), "tools/call", "task").await;
        // No follow-up traffic should be needed to persist task creation.
        tokio::time::sleep(Duration::from_millis(1400)).await;
        call(build_client(), binary.url.clone(), "tasks/get", "").await;
        if shutdown {
            binary.terminate();
            assert!(binary.exited().await);
        }
        until(|| state.accepted.lock().unwrap().values().sum::<u64>() == 7).await;
        if !shutdown {
            binary.terminate();
            assert!(binary.exited().await);
        }
        assert_eq!(state.accepted.lock().unwrap().values().sum::<u64>(), 7);
    }
}
