use crossbar::prelude::*;
use std::sync::atomic::{AtomicU16, Ordering};

// ── Helpers ────────────────────────────────────────────

static PORT_COUNTER: AtomicU16 = AtomicU16::new(19_000);

fn next_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn test_router() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/echo",
            post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
        )
        .route(
            "/json",
            get(|| async {
                #[derive(serde::Serialize)]
                struct Data {
                    msg: String,
                    num: i32,
                }
                Json(Data {
                    msg: "hello".into(),
                    num: 42,
                })
            }),
        )
        .route(
            "/status/:code",
            get(|req: Request| async move {
                let code: u16 = req
                    .path_param("code")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
                Response::with_status(code).with_body(format!("status:{code}"))
            }),
        )
}

fn uds_path(name: &str) -> String {
    format!("/tmp/crossbar-test-{name}-{}.sock", std::process::id())
}

// ═══════════════════════════════════════════════════════
// MemoryClient
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn memory_get() {
    let client = MemoryClient::new(test_router());
    let resp = client.get("/health").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[tokio::test]
async fn memory_post_with_body() {
    let client = MemoryClient::new(test_router());
    let resp = client.post("/echo", "hello memory").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello memory");
}

#[tokio::test]
async fn memory_json_response() {
    let client = MemoryClient::new(test_router());
    let resp = client.get("/json").await;
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["msg"], "hello");
    assert_eq!(v["num"], 42);
}

#[tokio::test]
async fn memory_404() {
    let client = MemoryClient::new(test_router());
    let resp = client.get("/nonexistent").await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn memory_empty_body() {
    let client = MemoryClient::new(test_router());
    let resp = client.post("/echo", "").await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn memory_binary_body_roundtrip() {
    let binary: Vec<u8> = (0..=255).collect();
    let client = MemoryClient::new(test_router());
    let resp = client.post("/echo", binary.clone()).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_ref(), binary.as_slice());
}

#[tokio::test]
async fn memory_large_payload() {
    let data = vec![b'X'; 1_000_000]; // 1 MB
    let client = MemoryClient::new(test_router());
    let resp = client.post("/echo", data.clone()).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.len(), 1_000_000);
    assert_eq!(resp.body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn memory_json_roundtrip() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Item {
        id: u64,
        name: String,
    }

    let router = Router::new().route(
        "/roundtrip",
        post(|req: Request| async move {
            let item: Item = req.json_body().unwrap();
            Json(item)
        }),
    );
    let client = MemoryClient::new(router);

    let input = Item {
        id: 123,
        name: "test".into(),
    };
    let body = serde_json::to_vec(&input).unwrap();
    let resp = client.post("/roundtrip", body).await;
    assert_eq!(resp.status, 200);
    let output: Item = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(input, output);
}

// ═══════════════════════════════════════════════════════
// ChannelClient/Server
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn channel_get() {
    let client = ChannelServer::spawn(test_router());
    let resp = client.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[tokio::test]
async fn channel_post() {
    let client = ChannelServer::spawn(test_router());
    let resp = client.post("/echo", "channel data").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "channel data");
}

#[tokio::test]
async fn channel_404() {
    let client = ChannelServer::spawn(test_router());
    let resp = client.get("/nope").await.unwrap();
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn channel_server_drop() {
    let client = ChannelServer::spawn(test_router());
    // The server runs in a background task; drop the only handle.
    // But ChannelClient owns the tx, so let's drop a clone scenario.
    // Actually, the server drops when all clients drop. Let's verify a cloned client works.
    let client2 = client.clone();
    let resp = client2.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
    drop(client);
    // client2 still works because the channel is still alive
    let resp = client2.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
}

#[tokio::test]
async fn channel_empty_body() {
    let client = ChannelServer::spawn(test_router());
    let resp = client.post("/echo", "").await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn channel_binary_roundtrip() {
    let binary: Vec<u8> = (0..=255).collect();
    let client = ChannelServer::spawn(test_router());
    let resp = client.post("/echo", binary.clone()).await.unwrap();
    assert_eq!(resp.body.as_ref(), binary.as_slice());
}

#[tokio::test]
async fn channel_large_payload() {
    let data = vec![b'Y'; 1_000_000];
    let client = ChannelServer::spawn(test_router());
    let resp = client.post("/echo", data.clone()).await.unwrap();
    assert_eq!(resp.body.len(), 1_000_000);
    assert_eq!(resp.body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn channel_json_roundtrip() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Payload {
        value: f64,
    }

    let router = Router::new().route(
        "/rpc",
        post(|req: Request| async move {
            let p: Payload = req.json_body().unwrap();
            Json(Payload {
                value: p.value * 2.0,
            })
        }),
    );
    let client = ChannelServer::spawn(router);
    let body = serde_json::to_vec(&Payload { value: 21.0 }).unwrap();
    let resp = client.post("/rpc", body).await.unwrap();
    let out: Payload = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(out.value, 42.0);
}

// ═══════════════════════════════════════════════════════
// UdsClient/Server
// ═══════════════════════════════════════════════════════

#[cfg(unix)]
#[tokio::test]
async fn uds_get() {
    let path = uds_path("uds_get");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = UdsClient::connect(&path).await.unwrap();
    let resp = client.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_post() {
    let path = uds_path("uds_post");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = UdsClient::connect(&path).await.unwrap();
    let resp = client.post("/echo", "uds body").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "uds body");

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_persistent_connection() {
    let path = uds_path("uds_persist");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = UdsClient::connect(&path).await.unwrap();

    // Multiple requests on the same connection
    for i in 0..10 {
        let resp = client.post("/echo", format!("msg-{i}")).await.unwrap();
        assert_eq!(resp.body_str(), format!("msg-{i}"));
    }

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_reconnect() {
    let path = uds_path("uds_reconnect");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First connection
    let client1 = UdsClient::connect(&path).await.unwrap();
    let resp = client1.get("/health").await.unwrap();
    assert_eq!(resp.body_str(), "ok");
    drop(client1);

    // Second connection (reconnect)
    let client2 = UdsClient::connect(&path).await.unwrap();
    let resp = client2.get("/health").await.unwrap();
    assert_eq!(resp.body_str(), "ok");

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_empty_body() {
    let path = uds_path("uds_empty");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = UdsClient::connect(&path).await.unwrap();
    let resp = client.post("/echo", "").await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_binary_roundtrip() {
    let path = uds_path("uds_binary");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let binary: Vec<u8> = (0..=255).collect();
    let client = UdsClient::connect(&path).await.unwrap();
    let resp = client.post("/echo", binary.clone()).await.unwrap();
    assert_eq!(resp.body.as_ref(), binary.as_slice());

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_large_payload() {
    let path = uds_path("uds_large");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let data = vec![b'Z'; 1_000_000];
    let client = UdsClient::connect(&path).await.unwrap();
    let resp = client.post("/echo", data.clone()).await.unwrap();
    assert_eq!(resp.body.len(), 1_000_000);

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_404() {
    let path = uds_path("uds_404");
    let router = test_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = UdsClient::connect(&path).await.unwrap();
    let resp = client.get("/nope").await.unwrap();
    assert_eq!(resp.status, 404);

    let _ = std::fs::remove_file(&path);
}

// ═══════════════════════════════════════════════════════
// TcpClient/Server
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn tcp_get() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[tokio::test]
async fn tcp_post() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.post("/echo", "tcp body").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "tcp body");
}

#[tokio::test]
async fn tcp_persistent_connection() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    for i in 0..10 {
        let resp = client.post("/echo", format!("tcp-{i}")).await.unwrap();
        assert_eq!(resp.body_str(), format!("tcp-{i}"));
    }
}

#[tokio::test]
async fn tcp_empty_body() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.post("/echo", "").await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn tcp_binary_roundtrip() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let binary: Vec<u8> = (0..=255).collect();
    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.post("/echo", binary.clone()).await.unwrap();
    assert_eq!(resp.body.as_ref(), binary.as_slice());
}

#[tokio::test]
async fn tcp_large_payload() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let data = vec![b'W'; 1_000_000];
    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.post("/echo", data.clone()).await.unwrap();
    assert_eq!(resp.body.len(), 1_000_000);
    assert_eq!(resp.body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn tcp_404() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = test_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.get("/missing").await.unwrap();
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn tcp_json_roundtrip() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Payload {
        value: String,
    }

    let router = Router::new().route(
        "/rpc",
        post(|req: Request| async move {
            let p: Payload = req.json_body().unwrap();
            Json(Payload {
                value: format!("echo:{}", p.value),
            })
        }),
    );

    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let body = serde_json::to_vec(&Payload {
        value: "test".into(),
    })
    .unwrap();
    let resp = client.post("/rpc", body).await.unwrap();
    let out: Payload = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(out.value, "echo:test");
}

// ═══════════════════════════════════════════════════════
// Cross-transport consistency
// ═══════════════════════════════════════════════════════

#[cfg(unix)]
#[tokio::test]
async fn all_transports_identical_responses() {
    let router = test_router();

    // Memory
    let mem_client = MemoryClient::new(router.clone());
    let mem_resp = mem_client.get("/health").await;

    // Channel
    let chan_client = ChannelServer::spawn(router.clone());
    let chan_resp = chan_client.get("/health").await.unwrap();

    // UDS
    let uds_p = uds_path("cross_transport");
    tokio::spawn({
        let path = uds_p.clone();
        let router = router.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let uds_client = UdsClient::connect(&uds_p).await.unwrap();
    let uds_resp = uds_client.get("/health").await.unwrap();

    // TCP
    let port = next_port();
    let tcp_addr = format!("127.0.0.1:{port}");
    tokio::spawn({
        let addr = tcp_addr.clone();
        let router = router.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let tcp_client = TcpClient::connect(&tcp_addr).await.unwrap();
    let tcp_resp = tcp_client.get("/health").await.unwrap();

    // All should return the same status and body
    assert_eq!(mem_resp.status, 200);
    assert_eq!(chan_resp.status, 200);
    assert_eq!(uds_resp.status, 200);
    assert_eq!(tcp_resp.status, 200);

    assert_eq!(mem_resp.body_str(), "ok");
    assert_eq!(chan_resp.body_str(), "ok");
    assert_eq!(uds_resp.body_str(), "ok");
    assert_eq!(tcp_resp.body_str(), "ok");

    let _ = std::fs::remove_file(&uds_p);
}

#[cfg(unix)]
#[tokio::test]
async fn all_transports_post_identical() {
    let router = test_router();
    let payload = "identical-payload";

    let mem = MemoryClient::new(router.clone());
    let mem_r = mem.post("/echo", payload).await;

    let chan = ChannelServer::spawn(router.clone());
    let chan_r = chan.post("/echo", payload).await.unwrap();

    let uds_p = uds_path("cross_post");
    tokio::spawn({
        let p = uds_p.clone();
        let r = router.clone();
        async move { UdsServer::bind(&p, r).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let uds_c = UdsClient::connect(&uds_p).await.unwrap();
    let uds_r = uds_c.post("/echo", payload).await.unwrap();

    let port = next_port();
    let tcp_a = format!("127.0.0.1:{port}");
    tokio::spawn({
        let a = tcp_a.clone();
        let r = router.clone();
        async move { TcpServer::bind(&a, r).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let tcp_c = TcpClient::connect(&tcp_a).await.unwrap();
    let tcp_r = tcp_c.post("/echo", payload).await.unwrap();

    for resp in [&mem_r, &chan_r, &uds_r, &tcp_r] {
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_str(), payload);
    }

    let _ = std::fs::remove_file(&uds_p);
}

// ═══════════════════════════════════════════════════════
// Headers survive wire roundtrip
// ═══════════════════════════════════════════════════════

/// Helper trait for method-chaining mutations on a value.
trait Pipe: Sized {
    fn pipe<F: FnOnce(Self) -> Self>(self, f: F) -> Self {
        f(self)
    }
}

impl Pipe for Request {}

/// Verify that request and response headers are faithfully transmitted over TCP.
#[tokio::test]
async fn tcp_headers_roundtrip() {
    // The handler echoes all request headers back as response headers under a
    // "x-echo-" prefix, and additionally sets its own "x-server" header.
    let router = Router::new().route(
        "/headers",
        get(|req: Request| async move {
            let mut resp = Response::ok().with_body("headers-ok");
            for (k, v) in &req.headers {
                resp = resp.with_header(format!("x-echo-{k}"), v.clone());
            }
            resp = resp.with_header("x-server", "crossbar");
            resp
        }),
    );

    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let req = Request::new(Method::Get, "/headers").pipe(|mut r| {
        r.headers
            .insert("x-request-id".to_string(), "abc123".to_string());
        r.headers
            .insert("content-type".to_string(), "text/plain".to_string());
        r
    });
    let resp = client.request(req).await.unwrap();

    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "headers-ok");
    assert_eq!(
        resp.headers.get("x-server").map(String::as_str),
        Some("crossbar")
    );
    assert_eq!(
        resp.headers.get("x-echo-x-request-id").map(String::as_str),
        Some("abc123")
    );
    assert_eq!(
        resp.headers.get("x-echo-content-type").map(String::as_str),
        Some("text/plain")
    );
}

/// Verify that response headers set by `Response::json` survive the TCP wire roundtrip.
#[tokio::test]
async fn tcp_json_content_type_header_roundtrip() {
    let router = Router::new().route(
        "/json-ct",
        get(|| async {
            #[derive(serde::Serialize)]
            struct Data {
                ok: bool,
            }
            Response::json(&Data { ok: true })
        }),
    );

    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = TcpClient::connect(&addr).await.unwrap();
    let resp = client.get("/json-ct").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.headers.get("content-type").map(String::as_str),
        Some("application/json"),
        "content-type header must survive the TCP wire roundtrip"
    );
}

// ═══════════════════════════════════════════════════════
// Frame-too-large rejection
// ═══════════════════════════════════════════════════════

/// Verify that `CrossbarError::FrameTooLarge` is returned when the declared
/// response frame size exceeds `MAX_FRAME_SIZE`.
///
/// We spin up a raw TCP listener that sends back a crafted response header
/// whose `body_len` exceeds the limit, then connect a [`TcpClient`] and
/// verify the error variant.
#[tokio::test]
async fn tcp_frame_too_large_rejected() {
    use crossbar::transport::MAX_FRAME_SIZE;
    use tokio::io::AsyncWriteExt;

    // Fake server: accepts one connection, reads and discards the request,
    // then sends back a response frame with an oversized body_len field.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut stream, _) = listener.accept().await.unwrap();

        // Drain the incoming request frame (at least the 13-byte header).
        let mut scratch = [0u8; 256];
        let _ = stream.read(&mut scratch).await;

        // Build a response header claiming body_len = MAX_FRAME_SIZE + 1.
        let oversized = MAX_FRAME_SIZE as u64 + 1;
        let mut resp_header = [0u8; 10];
        resp_header[0..2].copy_from_slice(&200u16.to_le_bytes()); // status 200
        resp_header[2..6].copy_from_slice(&(oversized as u32).to_le_bytes()); // body_len
        resp_header[6..10].copy_from_slice(&0u32.to_le_bytes()); // headers_data_len = 0
        stream.write_all(&resp_header).await.unwrap();
        stream.flush().await.unwrap();

        // Keep alive long enough for the client to read the header.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });

    let client = TcpClient::connect(&addr.to_string()).await.unwrap();
    let result = client.get("/anything").await;

    match result {
        Err(CrossbarError::FrameTooLarge { size, max }) => {
            assert_eq!(max, MAX_FRAME_SIZE);
            assert!(size > MAX_FRAME_SIZE, "size={size} should exceed max={max}");
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}
