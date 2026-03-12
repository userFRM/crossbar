use crossbar::prelude::*;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

// ── Helpers ────────────────────────────────────────────

static STRESS_PORT: AtomicU16 = AtomicU16::new(20_000);

fn next_port() -> u16 {
    STRESS_PORT.fetch_add(1, Ordering::SeqCst)
}

fn uds_path(name: &str) -> String {
    format!("/tmp/crossbar-stress-{name}-{}.sock", std::process::id())
}

fn echo_router() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/echo",
            post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
        )
}

// ═══════════════════════════════════════════════════════
// 100 concurrent memory requests
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn stress_memory_100_concurrent() {
    let client = Arc::new(MemoryClient::new(echo_router()));

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

// ═══════════════════════════════════════════════════════
// 50 concurrent UDS requests (separate clients per task)
// ═══════════════════════════════════════════════════════

#[cfg(unix)]
#[tokio::test]
async fn stress_uds_50_concurrent() {
    let path = uds_path("stress_uds_50");
    let router = echo_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut handles = Vec::new();
    for i in 0..50 {
        let path = path.clone();
        handles.push(tokio::spawn(async move {
            let client = UdsClient::connect(&path).await.unwrap();
            let resp = client.post("/echo", format!("uds-{i}")).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body_str(), format!("uds-{i}"));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let _ = std::fs::remove_file(&path);
}

// ═══════════════════════════════════════════════════════
// 50 concurrent TCP requests
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn stress_tcp_50_concurrent() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = echo_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut handles = Vec::new();
    for i in 0..50 {
        let addr = addr.clone();
        handles.push(tokio::spawn(async move {
            let client = TcpClient::connect(&addr).await.unwrap();
            let resp = client.post("/echo", format!("tcp-{i}")).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body_str(), format!("tcp-{i}"));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════
// Large number of routes (100+ routes, correct dispatch)
// ═══════════════════════════════════════════════════════

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

    let client = MemoryClient::new(router.clone());

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

// ═══════════════════════════════════════════════════════
// Rapid sequential requests on persistent connection (1000)
// ═══════════════════════════════════════════════════════

#[cfg(unix)]
#[tokio::test]
async fn stress_rapid_sequential_uds_1000() {
    let path = uds_path("stress_rapid_uds");
    let router = echo_router();

    tokio::spawn({
        let path = path.clone();
        async move { UdsServer::bind(&path, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let client = UdsClient::connect(&path).await.unwrap();

    for i in 0..1000 {
        let resp = client.get("/health").await.unwrap();
        assert_eq!(resp.status, 200, "request {i} failed");
        assert_eq!(resp.body_str(), "ok");
    }

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn stress_rapid_sequential_tcp_1000() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let router = echo_router();

    tokio::spawn({
        let addr = addr.clone();
        async move { TcpServer::bind(&addr, router).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let client = TcpClient::connect(&addr).await.unwrap();

    for i in 0..1000 {
        let resp = client.get("/health").await.unwrap();
        assert_eq!(resp.status, 200, "request {i} failed");
        assert_eq!(resp.body_str(), "ok");
    }
}

// ═══════════════════════════════════════════════════════
// Channel with multiple producers
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn stress_channel_multiple_producers() {
    let client = ChannelServer::spawn(echo_router());

    let mut handles = Vec::new();
    for i in 0..50 {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..10 {
                let resp = client
                    .post("/echo", format!("producer-{i}-msg-{j}"))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body_str(), format!("producer-{i}-msg-{j}"));
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════
// Concurrent channel requests (100)
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn stress_channel_100_concurrent() {
    let client = ChannelServer::spawn(echo_router());

    let mut handles = Vec::new();
    for i in 0..100 {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = client.post("/echo", format!("chan-{i}")).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body_str(), format!("chan-{i}"));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════
// Memory rapid sequential (1000)
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn stress_memory_rapid_1000() {
    let client = MemoryClient::new(echo_router());

    for i in 0..1000 {
        let resp = client.get("/health").await;
        assert_eq!(resp.status, 200, "request {i} failed");
    }
}
