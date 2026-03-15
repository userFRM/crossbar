use crossbar_inproc::prelude::*;
use std::sync::Arc;

// -- Helpers ----

fn echo_router() -> Router {
    Router::new().route("/health", get(|| "ok")).route(
        "/echo",
        post(|req: Request| Response::ok().with_body(req.body.clone())),
    )
}

// ===============================================
// 100 concurrent in-process requests
// ===============================================

#[test]
fn stress_inproc_100_concurrent() {
    let client = Arc::new(InProcessClient::new(echo_router()));

    let mut handles = Vec::new();
    for i in 0..100 {
        let client = Arc::clone(&client);
        handles.push(std::thread::spawn(move || {
            let resp = client.post("/echo", format!("msg-{i}"));
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body_str(), format!("msg-{i}"));
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

// ===============================================
// Large number of routes (100+ routes, correct dispatch)
// ===============================================

#[test]
fn stress_100_routes_dispatch() {
    let mut router = Router::new();
    for i in 0..150 {
        let path = format!("/route{i}");
        // We need to capture i into the handler. Use a closure that returns the index.
        router = router.route(
            Box::leak(path.into_boxed_str()),
            get(move || {
                let val = i;
                format!("handler-{val}")
            }),
        );
    }

    let client = InProcessClient::new(router.clone());

    // Test first, middle, and last routes
    for i in [0, 1, 50, 99, 100, 149] {
        let resp = client.get(&format!("/route{i}"));
        assert_eq!(resp.status, 200, "route{i} should match");
        assert_eq!(
            resp.body_str(),
            format!("handler-{i}"),
            "route{i} wrong handler"
        );
    }

    // Verify a non-existent route returns 404
    let resp = client.get("/route150");
    assert_eq!(resp.status, 404);

    // Verify routes_info count
    assert_eq!(router.routes_info().len(), 150);
}

// ===============================================
// In-process rapid sequential (1000)
// ===============================================

#[test]
fn stress_inproc_rapid_1000() {
    let client = InProcessClient::new(echo_router());

    for i in 0..1000 {
        let resp = client.get("/health");
        assert_eq!(resp.status, 200, "request {i} failed");
    }
}
