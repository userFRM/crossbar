use crossbar::prelude::*;

// -- Helpers ----

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

// ===============================================
// MemoryClient
// ===============================================

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

// ===============================================
// ShmClient/Server
// ===============================================

#[cfg(all(unix, feature = "shm"))]
fn shm_name(name: &str) -> String {
    format!("test-{name}-{}", std::process::id())
}

#[cfg(all(unix, feature = "shm"))]
fn cleanup_shm(name: &str) {
    let path = format!("/dev/shm/crossbar-{name}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_get() {
    let name = shm_name("shm_get");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();
    let resp = client.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_post() {
    let name = shm_name("shm_post");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();
    let resp = client.post("/echo", "shm body").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "shm body");

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_404() {
    let name = shm_name("shm_404");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();
    let resp = client.get("/nonexistent").await.unwrap();
    assert_eq!(resp.status, 404);

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_empty_body() {
    let name = shm_name("shm_empty");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();
    let resp = client.post("/echo", "").await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_binary_roundtrip() {
    let name = shm_name("shm_binary");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let binary: Vec<u8> = (0..=255).collect();
    let client = ShmClient::connect(&name).await.unwrap();
    let resp = client.post("/echo", binary.clone()).await.unwrap();
    assert_eq!(resp.body.as_ref(), binary.as_slice());

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_json_roundtrip() {
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

    let name = shm_name("shm_json");
    let _handle = ShmServer::spawn(&name, router).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();
    let body = serde_json::to_vec(&Payload {
        value: "test".into(),
    })
    .unwrap();
    let resp = client.post("/rpc", body).await.unwrap();
    let out: Payload = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(out.value, "echo:test");

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_headers_roundtrip() {
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

    let name = shm_name("shm_headers");
    let _handle = ShmServer::spawn(&name, router).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();
    let mut req = Request::new(Method::Get, "/headers");
    req.headers
        .insert("x-request-id".to_string(), "abc123".to_string());
    req.headers
        .insert("content-type".to_string(), "text/plain".to_string());
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

    cleanup_shm(&name);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_concurrent_requests() {
    let name = shm_name("shm_concurrent");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let client = std::sync::Arc::new(
        ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(30))
            .await
            .unwrap(),
    );

    let mut handles = Vec::new();
    for i in 0..50 {
        let client = std::sync::Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let resp = client.get("/health").await.unwrap();
            assert_eq!(resp.status, 200, "concurrent request {i} failed");
            assert_eq!(resp.body_str(), "ok");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    cleanup_shm(&name);
}

/// Verify that a client timeout on a slow handler does not corrupt slots.
///
/// The handler sleeps longer than the client's stale_timeout, so the client
/// gives up. A subsequent request must still succeed -- proving the
/// timed-out slot was NOT unsafely freed while the server was still using it.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_slow_handler_timeout_no_corruption() {
    use crossbar::prelude::*;

    let name = shm_name("shm_slow");

    // A router with one slow handler and one fast handler.
    let router = Router::new()
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                "done"
            }),
        )
        .route("/fast", get(|| async { "ok" }));

    let _handle = ShmServer::spawn(&name, router).await.unwrap();

    // Client with a very short timeout -- will give up before /slow returns.
    let client = ShmClient::connect_with_timeout(&name, std::time::Duration::from_millis(200))
        .await
        .unwrap();

    // This request should time out (server handler runs 600ms, timeout is 200ms).
    let result = client.get("/slow").await;
    assert!(result.is_err(), "expected timeout error from slow handler");

    // Wait for the server to finish processing the slow request and for
    // stale recovery to clean up the abandoned RESPONSE_READY slot.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    // Now make a fresh client with a generous timeout and verify the
    // system is still healthy -- no corruption from the timed-out slot.
    let client2 = ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    let resp = client2.get("/fast").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");

    cleanup_shm(&name);
}

// ===============================================
// SHM slots-full error
// ===============================================

/// Exhaust all SHM slots by holding responses, verify ShmSlotsFull error.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_slots_full_returns_error() {
    let name = shm_name("shm_slots_full");
    let config = ShmConfig {
        slot_count: 2,
        block_size: 4096,
        stale_timeout: std::time::Duration::from_secs(10),
        ..ShmConfig::default()
    };

    // Use a slow handler to keep slots occupied
    let router = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            "done"
        }),
    );

    let _handle = ShmServer::spawn_with_config(&name, router, config)
        .await
        .unwrap();

    let client = std::sync::Arc::new(
        ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(10))
            .await
            .unwrap(),
    );

    // Fire off requests that will block in the handler, occupying all slots
    let mut handles = Vec::new();
    for _ in 0..2 {
        let c = std::sync::Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let _ = c.get("/slow").await;
        }));
    }

    // Give the server time to pick up both requests (transition to PROCESSING)
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Third request should fail -- all slots are occupied
    let result = client.get("/slow").await;
    assert!(
        result.is_err(),
        "expected error when all slots are occupied"
    );

    // Clean up
    for h in handles {
        h.abort();
    }

    cleanup_shm(&name);
}

/// Verify response body (Bytes) remains valid after the coordination slot
/// transitions to FREE. The body is copied out of the slot before the slot
/// is released, so the Bytes buffer must remain independently valid.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_body_valid_after_slot_free() {
    let name = shm_name("shm_body_after_free");
    let _handle = ShmServer::spawn(&name, test_router()).await.unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    // Get a response with a known body
    let resp = client.post("/echo", "persistent-data").await.unwrap();
    assert_eq!(resp.status, 200);

    // The slot is now FREE (released by the client). The body should still be
    // valid because it was copied out before the slot was freed.
    let body_snapshot = resp.body.clone();

    // Make more requests to reuse slots, potentially overwriting slot data
    for _ in 0..10 {
        let _ = client.get("/health").await.unwrap();
    }

    // The original body must still be valid
    assert_eq!(resp.body_str(), "persistent-data");
    assert_eq!(body_snapshot.as_ref(), b"persistent-data");

    cleanup_shm(&name);
}

// ===============================================
// Cross-transport consistency
// ===============================================

/// Verify that the shm transport returns the exact same response as MemoryClient.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_matches_memory_responses() {
    let router = test_router();

    // Memory baseline
    let mem_client = MemoryClient::new(router.clone());
    let mem_get = mem_client.get("/health").await;
    let mem_post = mem_client.post("/echo", "identical-payload").await;
    let mem_json = mem_client.get("/json").await;
    let mem_404 = mem_client.get("/nonexistent").await;

    // Shm
    let name = shm_name("shm_cross");
    let _handle = ShmServer::spawn(&name, router).await.unwrap();
    let shm_client = ShmClient::connect(&name).await.unwrap();

    let shm_get = shm_client.get("/health").await.unwrap();
    let shm_post = shm_client.post("/echo", "identical-payload").await.unwrap();
    let shm_json = shm_client.get("/json").await.unwrap();
    let shm_404 = shm_client.get("/nonexistent").await.unwrap();

    // GET /health
    assert_eq!(mem_get.status, shm_get.status);
    assert_eq!(mem_get.body_str(), shm_get.body_str());

    // POST /echo
    assert_eq!(mem_post.status, shm_post.status);
    assert_eq!(mem_post.body_str(), shm_post.body_str());

    // GET /json
    assert_eq!(mem_json.status, shm_json.status);
    assert_eq!(mem_json.body.as_ref(), shm_json.body.as_ref());

    // GET /nonexistent -> 404
    assert_eq!(mem_404.status, shm_404.status);

    cleanup_shm(&name);
}
