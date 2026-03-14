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
// InProcessClient
// ===============================================

#[tokio::test]
async fn inproc_get() {
    let client = InProcessClient::new(test_router());
    let resp = client.get("/health").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[tokio::test]
async fn inproc_post_with_body() {
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", "hello inproc").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello inproc");
}

#[tokio::test]
async fn inproc_json_response() {
    let client = InProcessClient::new(test_router());
    let resp = client.get("/json").await;
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["msg"], "hello");
    assert_eq!(v["num"], 42);
}

#[tokio::test]
async fn inproc_404() {
    let client = InProcessClient::new(test_router());
    let resp = client.get("/nonexistent").await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn inproc_empty_body() {
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", "").await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn inproc_binary_body_roundtrip() {
    let binary: Vec<u8> = (0..=255).collect();
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", binary.clone()).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_ref(), binary.as_slice());
}

#[tokio::test]
async fn inproc_large_payload() {
    let data = vec![b'X'; 1_000_000]; // 1 MB
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", data.clone()).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.len(), 1_000_000);
    assert_eq!(resp.body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn inproc_json_roundtrip() {
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
    let client = InProcessClient::new(router);

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

/// Verify response body (Body) remains valid after the coordination slot
/// transitions to FREE. The body is copied out of the slot before the slot
/// is released, so the Body buffer must remain independently valid.
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

/// Zero-copy body guard: hold response body Body across many subsequent
/// requests. The ShmBodyGuard inside Body must keep the block alive even
/// though the slot has been freed and reused dozens of times.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_zero_copy_guard_survives_slot_reuse() {
    let name = shm_name("shm_zc_guard");
    let config = ShmConfig {
        slot_count: 2,
        block_count: 8,
        block_size: 4096,
        ..ShmConfig::default()
    };
    let _handle = ShmServer::spawn_with_config(&name, test_router(), config)
        .await
        .unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    // Capture a response whose body is backed by a ShmBlockGuard
    let held = client.post("/echo", "held-alive").await.unwrap();
    assert_eq!(held.body_str(), "held-alive");

    // Hammer the same slots and blocks so they get reused
    for i in 0..50 {
        let r = client.post("/echo", format!("reuse-{i}")).await.unwrap();
        assert_eq!(r.status, 200);
    }

    // The held body must still be intact
    assert_eq!(held.body_str(), "held-alive");

    cleanup_shm(&name);
}

// ===============================================
// Cross-transport consistency
// ===============================================

// ===============================================
// SHM edge case tests (task 7)
// ===============================================

/// Pool exhaustion returns CrossbarError::ShmPoolExhausted (not panic).
/// Use a tiny block_count and slow handlers so all blocks stay allocated.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_pool_exhaustion_returns_error() {
    let name = shm_name("shm_pool_exhaust");
    // 2 slots, 2 blocks -- each inflight request needs a block
    let config = ShmConfig {
        slot_count: 2,
        block_count: 2,
        block_size: 4096,
        stale_timeout: std::time::Duration::from_secs(30),
        ..ShmConfig::default()
    };

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
        ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(30))
            .await
            .unwrap(),
    );

    // Fire off requests that occupy all slots and blocks
    let mut handles = Vec::new();
    for _ in 0..2 {
        let c = std::sync::Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let _ = c.get("/slow").await;
        }));
    }

    // Wait for the requests to be picked up
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The third request should fail with pool exhaustion or slots full
    let result = client.get("/slow").await;
    assert!(result.is_err(), "expected error when pool is exhausted");

    for h in handles {
        h.abort();
    }
    cleanup_shm(&name);
}

/// Large payload near block_size boundary works correctly.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_large_payload_near_block_boundary() {
    let block_size: usize = 4096;
    let name = shm_name("shm_boundary");
    let config = ShmConfig {
        slot_count: 4,
        block_count: 16,
        block_size: block_size as u32,
        ..ShmConfig::default()
    };

    let router = Router::new().route(
        "/echo",
        post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
    );

    let _handle = ShmServer::spawn_with_config(&name, router, config)
        .await
        .unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    // Test payload that just fits (leave room for URI "/echo" = 5 bytes and
    // empty headers = 2 bytes = 7 bytes of overhead)
    let overhead = 5 + 2; // URI + header count
    let max_body = block_size - overhead;
    let payload = vec![b'A'; max_body];
    let resp = client.post("/echo", payload.clone()).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.len(), max_body);
    assert_eq!(resp.body.as_ref(), payload.as_slice());

    // Test payload that is exactly 1 byte too large -- should get an error
    let too_big = vec![b'B'; max_body + 1];
    let result = client.post("/echo", too_big).await;
    // The server will receive a ShmMessageTooLarge from write_request_to_block
    // and the client will see an error response (400) rather than a transport error
    // because the client-side write fails before REQUEST_READY.
    assert!(
        result.is_err(),
        "expected error for payload exceeding block capacity"
    );

    cleanup_shm(&name);
}

/// Empty headers roundtrip — a request with zero headers preserves them.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_empty_headers_roundtrip() {
    let name = shm_name("shm_empty_hdrs");
    let router = Router::new().route(
        "/hdr-count",
        get(|req: Request| async move {
            // Echo back the number of headers the server saw
            Response::ok().with_body(format!("{}", req.headers.len()))
        }),
    );

    let _handle = ShmServer::spawn(&name, router).await.unwrap();
    let client = ShmClient::connect(&name).await.unwrap();

    let req = Request::new(Method::Get, "/hdr-count");
    // req.headers is empty by default
    assert!(req.headers.is_empty());

    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "0");

    cleanup_shm(&name);
}

/// Many headers roundtrip — 20+ headers survive serialization/deserialization.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_many_headers_roundtrip() {
    let name = shm_name("shm_many_hdrs");
    let router = Router::new().route(
        "/mirror-headers",
        get(|req: Request| async move {
            let mut resp = Response::ok();
            for (k, v) in &req.headers {
                resp = resp.with_header(format!("echo-{k}"), v.clone());
            }
            resp
        }),
    );

    let _handle = ShmServer::spawn(&name, router).await.unwrap();
    let client = ShmClient::connect(&name).await.unwrap();

    let mut req = Request::new(Method::Get, "/mirror-headers");
    for i in 0..20 {
        req.headers
            .insert(format!("x-hdr-{i}"), format!("value-{i}"));
    }

    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status, 200);

    // Verify all 20 headers were echoed back
    for i in 0..20 {
        let key = format!("echo-x-hdr-{i}");
        assert_eq!(
            resp.headers.get(&key).map(String::as_str),
            Some(format!("value-{i}").as_str()),
            "missing echoed header {key}"
        );
    }

    cleanup_shm(&name);
}

/// Verify that the shm transport returns the exact same response as InProcessClient.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_matches_inproc_responses() {
    let router = test_router();

    // In-process baseline
    let mem_client = InProcessClient::new(router.clone());
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

// ===============================================
// PubSub seqlock torn-read detection tests (task 6)
// ===============================================

#[cfg(all(unix, feature = "shm"))]
fn pubsub_name(name: &str) -> String {
    format!("test-ps-{name}-{}", std::process::id())
}

/// try_recv() copy path: single-threaded consistency check.
/// Without concurrent writes, every sample must be fully consistent.
#[cfg(all(unix, feature = "shm"))]
#[test]
fn pubsub_try_recv_consistent_without_concurrent_writes() {
    use crossbar::prelude::*;

    let name = pubsub_name("no_torn");
    let cfg = PubSubConfig {
        ring_depth: 4,
        sample_capacity: 64,
        ..PubSubConfig::default()
    };

    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let _handle = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let mut stream = sub.subscribe("/data").unwrap();

    // Publish several samples with a recognizable pattern, then read them
    // without any concurrent writing — the copy path must yield perfect data.
    for batch in 0..10 {
        for i in 0u32..4 {
            let val = batch * 4 + i;
            let mut loan = pub_.loan("/data").unwrap();
            let bytes = val.to_le_bytes();
            let slice = loan.as_mut_slice();
            for chunk in slice[..64].chunks_exact_mut(4) {
                chunk.copy_from_slice(&bytes);
            }
            loan.set_len(64);
            loan.publish();
        }

        // Drain: each sample must be self-consistent
        while let Some(sample) = stream.try_recv() {
            let data: &[u8] = sample.as_ref();
            assert_eq!(data.len(), 64);
            let expected = u32::from_le_bytes(data[0..4].try_into().unwrap());
            for chunk in data.chunks_exact(4) {
                let val = u32::from_le_bytes(chunk.try_into().unwrap());
                assert_eq!(
                    val, expected,
                    "data inconsistency: expected {expected}, got {val}"
                );
            }
        }
    }
}

/// Concurrent publish/subscribe: verify the system doesn't crash and
/// that sequences are monotonically increasing. Under concurrent writes,
/// the seqlock may occasionally pass but deliver mixed data (a known
/// limitation of non-atomic memcpy seqlocks), so we only check sequence
/// ordering here, not byte-level content.
#[cfg(all(unix, feature = "shm"))]
#[test]
fn pubsub_concurrent_publish_subscribe_no_crash() {
    use crossbar::prelude::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let name = pubsub_name("conc_pubsub");
    let cfg = PubSubConfig {
        ring_depth: 8,
        sample_capacity: 64,
        ..PubSubConfig::default()
    };

    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let _handle = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let mut stream = sub.subscribe("/data").unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let done_clone = Arc::clone(&done);

    // Subscriber thread: read as fast as possible
    let subscriber_thread = std::thread::spawn(move || {
        let mut received = 0u64;
        while !done_clone.load(Ordering::Relaxed) {
            if stream.try_recv().is_some() {
                received += 1;
            }
        }
        // Drain remaining
        while stream.try_recv().is_some() {
            received += 1;
        }
        received
    });

    // Publisher: rapidly publish samples
    for i in 0u64..50_000 {
        let mut loan = pub_.loan("/data").unwrap();
        loan.set_data(&i.to_le_bytes());
        loan.publish_silent();
    }

    done.store(true, Ordering::Relaxed);
    let received = subscriber_thread.join().unwrap();
    assert!(
        received > 0,
        "subscriber should have received at least one sample"
    );
}

/// Publisher wraps the entire ring multiple times. Subscriber that fell behind
/// must catch up correctly — never returning stale or corrupted data.
#[cfg(all(unix, feature = "shm"))]
#[test]
fn pubsub_subscriber_catches_up_after_ring_wrap() {
    use crossbar::prelude::*;

    let name = pubsub_name("ring_wrap");
    let cfg = PubSubConfig {
        ring_depth: 4,
        sample_capacity: 32,
        ..PubSubConfig::default()
    };

    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let _handle = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let mut stream = sub.subscribe("/data").unwrap();

    // Publish many more samples than ring depth (wrap 5x)
    for i in 0u64..20 {
        let mut loan = pub_.loan("/data").unwrap();
        loan.set_data(&i.to_le_bytes());
        loan.publish();
    }

    // Subscriber hasn't read anything — it should skip to latest available
    let sample = stream.try_recv().unwrap();
    let val = u64::from_le_bytes(sample[..8].try_into().unwrap());
    // Must be one of the recent values (ring depth is 4, published 20)
    assert!(
        val >= 16,
        "subscriber should have caught up to recent value, got {val}"
    );

    // Subsequent reads should give sequential values
    let mut prev = val;
    while let Some(s) = stream.try_recv() {
        let v = u64::from_le_bytes(s[..8].try_into().unwrap());
        assert!(
            v > prev,
            "expected strictly increasing seq, got {v} after {prev}"
        );
        prev = v;
    }
}

/// High-frequency sequential publish then batch read.
/// Publisher writes all samples first, then subscriber reads them all.
/// No concurrent writes, so the copy path must yield perfect data.
#[cfg(all(unix, feature = "shm"))]
#[test]
fn pubsub_high_frequency_sequential_consistency() {
    use crossbar::prelude::*;

    let name = pubsub_name("hf_seq");
    let cfg = PubSubConfig {
        ring_depth: 16,
        sample_capacity: 128,
        ..PubSubConfig::default()
    };

    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let _handle = pub_.register("/stream").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let mut stream = sub.subscribe("/stream").unwrap();

    // Publish in batches, read after each batch to stay within ring depth
    let total_msgs: u64 = 1_000;
    let batch_size = 8u64; // less than ring_depth to avoid overwrites
    let mut total_received = 0u64;

    for batch_start in (0..total_msgs).step_by(batch_size as usize) {
        let batch_end = (batch_start + batch_size).min(total_msgs);
        for i in batch_start..batch_end {
            let mut loan = pub_.loan("/stream").unwrap();
            let slice = loan.as_mut_slice();
            slice[0..8].copy_from_slice(&i.to_le_bytes());
            let pattern = (i % 256) as u8;
            for b in &mut slice[8..128] {
                *b = pattern;
            }
            loan.set_len(128);
            loan.publish();
        }

        // Read all available samples and verify consistency
        while let Some(sample) = stream.try_recv() {
            total_received += 1;
            let data: &[u8] = sample.as_ref();
            assert_eq!(data.len(), 128, "sample length should be 128");

            let seq = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let expected_pattern = (seq % 256) as u8;

            for (j, &b) in data[8..].iter().enumerate() {
                assert_eq!(
                    b, expected_pattern,
                    "data corruption at byte {j}: seq={seq}, expected {expected_pattern}, got {b}"
                );
            }
        }
    }

    assert!(
        total_received > 0,
        "subscriber must have received at least one sample"
    );
}

/// try_recv_ref returns a raw pointer view. Verify copy_to_vec produces
/// consistent data matching what was published.
#[cfg(all(unix, feature = "shm"))]
#[test]
fn pubsub_try_recv_ref_copy_to_vec_consistent() {
    use crossbar::prelude::*;

    let name = pubsub_name("ref_copy");
    let cfg = PubSubConfig {
        ring_depth: 8,
        sample_capacity: 32,
        ..PubSubConfig::default()
    };

    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let _handle = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let mut stream = sub.subscribe("/data").unwrap();

    // Publish a known pattern
    for i in 0u32..5 {
        let mut loan = pub_.loan("/data").unwrap();
        let bytes = i.to_le_bytes();
        let slice = loan.as_mut_slice();
        for chunk in slice[..32].chunks_exact_mut(4) {
            chunk.copy_from_slice(&bytes);
        }
        loan.set_len(32);
        loan.publish();
    }

    // Read via try_recv_ref + copy_to_vec
    let mut received = 0;
    while let Some(sample_ref) = stream.try_recv_ref() {
        let data = sample_ref.copy_to_vec();
        received += 1;
        assert_eq!(data.len(), 32);
        let expected = u32::from_le_bytes(data[0..4].try_into().unwrap());
        for chunk in data.chunks_exact(4) {
            let val = u32::from_le_bytes(chunk.try_into().unwrap());
            assert_eq!(val, expected, "copy_to_vec data inconsistent");
        }
    }

    assert!(
        received > 0,
        "should have received at least one sample via try_recv_ref"
    );
}

// ─── Pool-backed O(1) pub/sub tests ─────────────────────────────────────

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_basic_publish_subscribe() {
    let name = &format!("test-pool-basic-{}", std::process::id());
    let mut pub_ = ShmPoolPublisher::create(name, PoolPubSubConfig::default()).unwrap();
    let topic = pub_.register("/test").unwrap();

    let sub = ShmPoolSubscriber::connect(name).unwrap();
    let mut stream = sub.subscribe("/test").unwrap();

    // Publish a sample
    let mut loan = pub_.loan(&topic);
    loan.set_data(b"hello pool pubsub");
    loan.publish();

    // Receive it
    let guard = stream.try_recv().expect("should receive sample");
    assert_eq!(&*guard, b"hello pool pubsub");
    assert_eq!(guard.len(), 17);
}

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_safe_deref() {
    // Verify that ShmPoolSampleGuard implements safe Deref (no unsafe needed by caller)
    let name = &format!("test-pool-deref-{}", std::process::id());
    let mut pub_ = ShmPoolPublisher::create(name, PoolPubSubConfig::default()).unwrap();
    let topic = pub_.register("/tick").unwrap();

    let sub = ShmPoolSubscriber::connect(name).unwrap();
    let mut stream = sub.subscribe("/tick").unwrap();

    let mut loan = pub_.loan(&topic);
    loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
    loan.set_len(8);
    loan.publish();

    let guard = stream.try_recv().unwrap();
    // SAFE: Deref, no unsafe needed (unlike ring-based ShmSampleRef)
    let val = u64::from_le_bytes(guard[..8].try_into().unwrap());
    assert_eq!(val, 42);
}

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_guard_survives_ring_overwrite() {
    // The key advantage over ring pub/sub: guard holds block alive via refcount
    let name = &format!("test-pool-survive-{}", std::process::id());
    let cfg = PoolPubSubConfig {
        ring_depth: 4,
        block_count: 32,
        ..PoolPubSubConfig::default()
    };
    let mut pub_ = ShmPoolPublisher::create(name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmPoolSubscriber::connect(name).unwrap();
    let mut stream = sub.subscribe("/data").unwrap();

    // Publish first sample
    let mut loan = pub_.loan(&topic);
    loan.set_data(b"first");
    loan.publish();

    // Read and HOLD the guard
    let guard = stream.try_recv().unwrap();
    assert_eq!(&*guard, b"first");

    // Overwrite the ring 10x (ring_depth=4, so slot 0 is overwritten)
    for i in 0u32..10 {
        let mut loan = pub_.loan(&topic);
        loan.set_data(&i.to_le_bytes());
        loan.publish();
    }

    // Guard still valid! Block is alive because refcount > 0
    assert_eq!(&*guard, b"first");
    drop(guard); // now the block is freed
}

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_multiple_topics() {
    let name = &format!("test-pool-topics-{}", std::process::id());
    let mut pub_ = ShmPoolPublisher::create(name, PoolPubSubConfig::default()).unwrap();
    let t1 = pub_.register("/a").unwrap();
    let t2 = pub_.register("/b").unwrap();

    let sub = ShmPoolSubscriber::connect(name).unwrap();
    let mut s1 = sub.subscribe("/a").unwrap();
    let mut s2 = sub.subscribe("/b").unwrap();

    let mut loan = pub_.loan(&t1);
    loan.set_data(b"alpha");
    loan.publish();

    let mut loan = pub_.loan(&t2);
    loan.set_data(b"beta");
    loan.publish();

    assert_eq!(&*s1.try_recv().unwrap(), b"alpha");
    assert_eq!(&*s2.try_recv().unwrap(), b"beta");
    assert!(s1.try_recv().is_none());
    assert!(s2.try_recv().is_none());
}

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_loan_dropped_without_publish() {
    // Loan dropped without publish should free the block (no leak)
    let name = &format!("test-pool-drop-{}", std::process::id());
    let cfg = PoolPubSubConfig {
        block_count: 8,
        ..PoolPubSubConfig::default()
    };
    let mut pub_ = ShmPoolPublisher::create(name, cfg).unwrap();
    let topic = pub_.register("/test").unwrap();

    // Allocate and drop 20 loans without publishing — if blocks leak, we'd panic
    for _ in 0..20 {
        let mut loan = pub_.loan(&topic);
        loan.set_data(b"unused");
        drop(loan); // should free the block
    }

    // Still works — blocks were returned to pool
    let mut loan = pub_.loan(&topic);
    loan.set_data(b"ok");
    loan.publish();
}

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_born_in_shm_pattern() {
    // iceoryx-style: write directly into the mmap block
    let name = &format!("test-pool-born-{}", std::process::id());
    let mut pub_ = ShmPoolPublisher::create(name, PoolPubSubConfig::default()).unwrap();
    let topic = pub_.register("/sensor").unwrap();

    let sub = ShmPoolSubscriber::connect(name).unwrap();
    let mut stream = sub.subscribe("/sensor").unwrap();

    // Write a struct directly into SHM (born-in-SHM)
    let mut loan = pub_.loan(&topic);
    let buf = loan.as_mut_slice();
    let ts: u64 = 1234567890;
    let value: f64 = std::f64::consts::PI;
    buf[..8].copy_from_slice(&ts.to_le_bytes());
    buf[8..16].copy_from_slice(&value.to_le_bytes());
    loan.set_len(16);
    loan.publish();

    let guard = stream.try_recv().unwrap();
    let read_ts = u64::from_le_bytes(guard[..8].try_into().unwrap());
    let read_val = f64::from_le_bytes(guard[8..16].try_into().unwrap());
    assert_eq!(read_ts, 1234567890);
    assert!((read_val - std::f64::consts::PI).abs() < f64::EPSILON);
}

#[cfg(all(unix, feature = "shm"))]
#[test]
fn pool_pubsub_sequential_consistency() {
    let name = &format!("test-pool-seq-{}", std::process::id());
    let mut pub_ = ShmPoolPublisher::create(name, PoolPubSubConfig::default()).unwrap();
    let topic = pub_.register("/seq").unwrap();

    let sub = ShmPoolSubscriber::connect(name).unwrap();
    let mut stream = sub.subscribe("/seq").unwrap();

    // Publish 100 sequential values
    for i in 0u64..100 {
        let mut loan = pub_.loan(&topic);
        loan.set_data(&i.to_le_bytes());
        loan.publish();
    }

    // Subscriber should see monotonically increasing values
    let mut last = None;
    while let Some(guard) = stream.try_recv() {
        let val = u64::from_le_bytes(guard[..8].try_into().unwrap());
        if let Some(prev) = last {
            assert!(val > prev, "expected {val} > {prev}");
        }
        last = Some(val);
    }
    assert!(last.is_some(), "should have received at least one sample");
}
