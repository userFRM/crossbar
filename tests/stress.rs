use crossbar::prelude::*;
use std::sync::Arc;

// -- Helpers ----

fn echo_router() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/echo",
            post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
        )
}

// ===============================================
// 100 concurrent in-process requests
// ===============================================

#[tokio::test]
async fn stress_inproc_100_concurrent() {
    let client = Arc::new(InProcessClient::new(echo_router()));

    let mut handles = Vec::new();
    for i in 0..100 {
        let client = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let resp = client.post("/echo", format!("msg-{i}")).await;
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body_str(), format!("msg-{i}"));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

// ===============================================
// Large number of routes (100+ routes, correct dispatch)
// ===============================================

#[tokio::test]
async fn stress_100_routes_dispatch() {
    let mut router = Router::new();
    for i in 0..150 {
        let path = format!("/route{i}");
        // We need to capture i into the handler. Use a closure that returns the index.
        router = router.route(
            Box::leak(path.into_boxed_str()),
            get(move || {
                let val = i;
                async move { format!("handler-{val}") }
            }),
        );
    }

    let client = InProcessClient::new(router.clone());

    // Test first, middle, and last routes
    for i in [0, 1, 50, 99, 100, 149] {
        let resp = client.get(&format!("/route{i}")).await;
        assert_eq!(resp.status, 200, "route{i} should match");
        assert_eq!(
            resp.body_str(),
            format!("handler-{i}"),
            "route{i} wrong handler"
        );
    }

    // Verify a non-existent route returns 404
    let resp = client.get("/route150").await;
    assert_eq!(resp.status, 404);

    // Verify routes_info count
    assert_eq!(router.routes_info().len(), 150);
}

// ===============================================
// In-process rapid sequential (1000)
// ===============================================

#[tokio::test]
async fn stress_inproc_rapid_1000() {
    let client = InProcessClient::new(echo_router());

    for i in 0..1000 {
        let resp = client.get("/health").await;
        assert_eq!(resp.status, 200, "request {i} failed");
    }
}

// ===============================================
// 50 concurrent SHM requests
// ===============================================

#[cfg(all(unix, feature = "shm"))]
fn shm_name(name: &str) -> String {
    format!("stress-{name}-{}", std::process::id())
}

#[cfg(all(unix, feature = "shm"))]
fn cleanup_shm(name: &str) {
    let path = format!("/dev/shm/crossbar-{name}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_shm_50_concurrent() {
    let name = shm_name("shm_50");
    let _handle = ShmServer::spawn(&name, echo_router()).await.unwrap();

    let client = Arc::new(
        ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(60))
            .await
            .unwrap(),
    );

    let mut handles = Vec::new();
    for i in 0..50 {
        let client = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let resp = client.post("/echo", format!("shm-{i}")).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body_str(), format!("shm-{i}"));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    cleanup_shm(&name);
}

// ===============================================
// Rapid sequential SHM requests (1000)
// ===============================================

// ===============================================
// Concurrent SHM slot allocation
// ===============================================

/// Multiple clients racing for slots simultaneously.
/// Verifies that concurrent CAS-based slot acquisition is safe.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_concurrent_pool_allocation() {
    let name = shm_name("shm_pool_alloc");
    let config = ShmConfig {
        slot_count: 8,
        block_size: 4096,
        ..ShmConfig::default()
    };

    let _handle = ShmServer::spawn_with_config(&name, echo_router(), config)
        .await
        .unwrap();

    // Create multiple independent clients to maximize contention
    let mut handles = Vec::new();
    for i in 0..20 {
        let name = name.clone();
        handles.push(tokio::spawn(async move {
            let client = ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(30))
                .await
                .unwrap();
            for j in 0..10 {
                let resp = client
                    .post("/echo", format!("client-{i}-msg-{j}"))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body_str(), format!("client-{i}-msg-{j}"));
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    cleanup_shm(&name);
}

// ===============================================
// Rapid sequential SHM requests (1000)
// ===============================================

#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_rapid_sequential_shm_1000() {
    let name = shm_name("shm_rapid");
    let _handle = ShmServer::spawn(&name, echo_router()).await.unwrap();

    let client = ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(60))
        .await
        .unwrap();

    for i in 0..1000 {
        let resp = client.get("/health").await.unwrap();
        assert_eq!(resp.status, 200, "request {i} failed");
        assert_eq!(resp.body_str(), "ok");
    }

    cleanup_shm(&name);
}

// ===============================================
// Block-free error path tests (task 5)
// ===============================================

/// Verify that blocks are returned to the pool after server error responses.
/// With a tiny pool (4 blocks), if blocks leaked on error paths, we'd exhaust
/// the pool after just 4 requests. Doing 100 sequential requests proves blocks
/// are correctly freed after each error.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_blocks_freed_after_404_errors() {
    let name = shm_name("blk_free_404");
    let config = ShmConfig {
        slot_count: 4,
        block_count: 4,
        block_size: 4096,
        ..ShmConfig::default()
    };

    let router = Router::new().route("/health", get(|| async { "ok" }));
    let _handle = ShmServer::spawn_with_config(&name, router, config)
        .await
        .unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    // 404 responses still allocate blocks for the response. If blocks leak,
    // this loop will fail with ShmPoolExhausted after 4 iterations.
    for i in 0..100 {
        let resp = client.get("/nonexistent").await.unwrap();
        assert_eq!(resp.status, 404, "request {i} should be 404");
    }

    // Verify the pool is still functional by making a successful request
    let resp = client.get("/health").await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");

    cleanup_shm(&name);
}

/// Verify blocks are freed correctly on successful request/response cycles
/// with a very small pool. This would fail if any code path leaked blocks.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_blocks_freed_after_success() {
    let name = shm_name("blk_free_ok");
    let config = ShmConfig {
        slot_count: 2,
        block_count: 4, // very tight pool
        block_size: 4096,
        ..ShmConfig::default()
    };

    let _handle = ShmServer::spawn_with_config(&name, echo_router(), config)
        .await
        .unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    // With only 4 blocks and each request needing 2 blocks (request + response),
    // we can only have ~2 in-flight. Sequential requests should work indefinitely
    // if blocks are properly freed.
    for i in 0..200 {
        let payload = format!("msg-{i}");
        let resp = client.post("/echo", payload.clone()).await.unwrap();
        assert_eq!(resp.status, 200, "request {i} failed");
        assert_eq!(resp.body_str(), payload.as_str());
    }

    cleanup_shm(&name);
}

/// Mix of successful and error responses with a tiny pool.
/// Exercises both success and error block-freeing paths.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_blocks_freed_mixed_success_and_error() {
    let name = shm_name("blk_free_mixed");
    let config = ShmConfig {
        slot_count: 2,
        block_count: 4,
        block_size: 4096,
        ..ShmConfig::default()
    };

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/echo",
            post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
        );

    let _handle = ShmServer::spawn_with_config(&name, router, config)
        .await
        .unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    for i in 0..100 {
        if i % 3 == 0 {
            // 404 error path
            let resp = client.get("/nonexistent").await.unwrap();
            assert_eq!(resp.status, 404, "request {i} should be 404");
        } else if i % 3 == 1 {
            // Successful GET
            let resp = client.get("/health").await.unwrap();
            assert_eq!(resp.status, 200, "request {i} should be 200");
        } else {
            // Successful POST
            let resp = client.post("/echo", "data").await.unwrap();
            assert_eq!(resp.status, 200, "request {i} should be 200");
        }
    }

    cleanup_shm(&name);
}

/// Empty body requests (body_len=0) with a tiny pool. The block must be freed
/// immediately in read_request_from_block when body is empty.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_blocks_freed_empty_body() {
    let name = shm_name("blk_free_empty");
    let config = ShmConfig {
        slot_count: 2,
        block_count: 4,
        block_size: 4096,
        ..ShmConfig::default()
    };

    let router = Router::new().route("/health", get(|| async { "ok" }));
    let _handle = ShmServer::spawn_with_config(&name, router, config)
        .await
        .unwrap();

    let client = ShmClient::connect(&name).await.unwrap();

    // GET requests send empty body -- the request block should be freed
    // immediately by read_request_from_block. With 4 blocks total, any leak
    // would stall after a few iterations.
    for i in 0..200 {
        let resp = client.get("/health").await.unwrap();
        assert_eq!(resp.status, 200, "request {i} failed");
    }

    cleanup_shm(&name);
}

// ===============================================
// Concurrent corruption test (task 7)
// ===============================================

/// Multiple clients sending concurrent requests must never corrupt each
/// other's responses. Each client sends a unique payload and verifies it.
#[cfg(all(unix, feature = "shm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_concurrent_no_response_corruption() {
    let name = shm_name("shm_no_corrupt");
    let config = ShmConfig {
        slot_count: 8,
        block_count: 32,
        block_size: 4096,
        ..ShmConfig::default()
    };

    let router = Router::new().route(
        "/echo",
        post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
    );

    let _handle = ShmServer::spawn_with_config(&name, router, config)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for client_id in 0..10 {
        let name = name.clone();
        handles.push(tokio::spawn(async move {
            let client = ShmClient::connect_with_timeout(&name, std::time::Duration::from_secs(30))
                .await
                .unwrap();
            for msg_id in 0..20 {
                let payload = format!("client-{client_id}-msg-{msg_id}");
                let resp = client.post("/echo", payload.clone()).await.unwrap();
                assert_eq!(resp.status, 200);
                assert_eq!(
                    resp.body_str(),
                    payload.as_str(),
                    "response corruption: client {client_id} msg {msg_id}"
                );
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    cleanup_shm(&name);
}
