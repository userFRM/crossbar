use crossbar::prelude::*;

// ── Helpers ────────────────────────────────────────────

fn memory(router: Router) -> MemoryClient {
    MemoryClient::new(router)
}

// ═══════════════════════════════════════════════════════
// 0-arg handler: async fn() -> impl IntoResponse
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_0_arg_returns_str() {
    async fn handler() -> &'static str {
        "hello"
    }

    let router = Router::new().route("/test", get(handler));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello");
}

#[tokio::test]
async fn handler_0_arg_returns_string() {
    async fn handler() -> String {
        String::from("dynamic")
    }

    let router = Router::new().route("/test", get(handler));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "dynamic");
}

#[tokio::test]
async fn handler_0_arg_returns_response() {
    async fn handler() -> Response {
        Response::with_status(201).with_body("created")
    }

    let router = Router::new().route("/test", post(handler));
    let client = memory(router);
    let resp = client.post("/test", "").await;
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created");
}

#[tokio::test]
async fn handler_0_arg_returns_tuple() {
    async fn handler() -> (u16, &'static str) {
        (202, "accepted")
    }

    let router = Router::new().route("/test", get(handler));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 202);
    assert_eq!(resp.body_str(), "accepted");
}

// ═══════════════════════════════════════════════════════
// 1-arg handler: async fn(Request) -> impl IntoResponse
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_1_arg_echoes_body() {
    async fn handler(req: Request) -> Response {
        Response::ok().with_body(req.body.clone())
    }

    let router = Router::new().route("/echo", post(handler));
    let client = memory(router);
    let resp = client.post("/echo", "test data").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "test data");
}

#[tokio::test]
async fn handler_1_arg_reads_path_param() {
    async fn handler(req: Request) -> String {
        format!("user:{}", req.path_param("id").unwrap_or("?"))
    }

    let router = Router::new().route("/users/:id", get(handler));
    let client = memory(router);
    let resp = client.get("/users/99").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "user:99");
}

#[tokio::test]
async fn handler_1_arg_reads_query() {
    async fn handler(req: Request) -> String {
        req.query_param("q").unwrap_or_default()
    }

    let router = Router::new().route("/search", get(handler));
    let client = memory(router);
    let resp = client.get("/search?q=hello").await;
    assert_eq!(resp.body_str(), "hello");
}

// ═══════════════════════════════════════════════════════
// Handler returning Json<T>
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_returns_json() {
    #[derive(serde::Serialize)]
    struct User {
        name: String,
        age: u32,
    }

    async fn handler() -> Json<User> {
        Json(User {
            name: "Alice".into(),
            age: 30,
        })
    }

    let router = Router::new().route("/user", get(handler));
    let client = memory(router);
    let resp = client.get("/user").await;
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["name"], "Alice");
    assert_eq!(v["age"], 30);
}

#[tokio::test]
async fn handler_returns_json_array() {
    async fn handler() -> Json<Vec<i32>> {
        Json(vec![1, 2, 3])
    }

    let router = Router::new().route("/nums", get(handler));
    let client = memory(router);
    let resp = client.get("/nums").await;
    let v: Vec<i32> = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v, vec![1, 2, 3]);
}

// ═══════════════════════════════════════════════════════
// Handler returning Result<Ok, Err>
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_returns_result_ok() {
    async fn handler(req: Request) -> Result<String, (u16, &'static str)> {
        let id = req.path_param("id").unwrap_or("0");
        Ok(format!("found:{id}"))
    }

    let router = Router::new().route("/items/:id", get(handler));
    let client = memory(router);
    let resp = client.get("/items/5").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "found:5");
}

#[tokio::test]
async fn handler_returns_result_err() {
    async fn handler(_req: Request) -> Result<&'static str, (u16, &'static str)> {
        Err((400, "bad input"))
    }

    let router = Router::new().route("/fail", get(handler));
    let client = memory(router);
    let resp = client.get("/fail").await;
    assert_eq!(resp.status, 400);
    assert_eq!(resp.body_str(), "bad input");
}

// ═══════════════════════════════════════════════════════
// Handler returning (u16, &str) tuple
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_returns_status_tuple() {
    async fn handler() -> (u16, &'static str) {
        (418, "I'm a teapot")
    }

    let router = Router::new().route("/teapot", get(handler));
    let client = memory(router);
    let resp = client.get("/teapot").await;
    assert_eq!(resp.status, 418);
    assert_eq!(resp.body_str(), "I'm a teapot");
}

#[tokio::test]
async fn handler_returns_status_tuple_string() {
    async fn handler() -> (u16, String) {
        (503, "service unavailable".to_string())
    }

    let router = Router::new().route("/status", get(handler));
    let client = memory(router);
    let resp = client.get("/status").await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body_str(), "service unavailable");
}

// ═══════════════════════════════════════════════════════
// Handler returning String
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_returns_owned_string() {
    async fn handler() -> String {
        format!("computed: {}", 42)
    }

    let router = Router::new().route("/computed", get(handler));
    let client = memory(router);
    let resp = client.get("/computed").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "computed: 42");
}

// ═══════════════════════════════════════════════════════
// BoxedHandler clone and call (tested via Router since call is pub(crate))
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn boxed_handler_clone_via_router() {
    // BoxedHandler is Clone (uses Arc internally). Test that a cloned router
    // (which clones the internal Arc<Vec<Route>>) still dispatches correctly.
    let router = Router::new().route("/test", get(|| async { "cloned" }));
    let cloned = router.clone();

    let client = memory(cloned);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "cloned");
}

#[tokio::test]
async fn boxed_handler_multiple_calls_via_router() {
    let router = Router::new().route("/test", get(|| async { "multi" }));
    let client = memory(router);

    for _ in 0..5 {
        let resp = client.get("/test").await;
        assert_eq!(resp.body_str(), "multi");
    }
}

// ═══════════════════════════════════════════════════════
// Closure handlers (not just fn items)
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn closure_handler_0_arg() {
    let router = Router::new().route("/test", get(|| async { "closure" }));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.body_str(), "closure");
}

#[tokio::test]
async fn closure_handler_1_arg() {
    let router = Router::new().route(
        "/echo",
        post(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
    );
    let client = memory(router);
    let resp = client.post("/echo", "data").await;
    assert_eq!(resp.body_str(), "data");
}

// ═══════════════════════════════════════════════════════
// Handler with JSON body deserialization
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn handler_deserializes_json_body() {
    #[derive(serde::Deserialize)]
    struct Input {
        x: i32,
        y: i32,
    }

    async fn handler(req: Request) -> String {
        match req.json_body::<Input>() {
            Ok(input) => format!("sum={}", input.x + input.y),
            Err(e) => format!("error: {e}"),
        }
    }

    let router = Router::new().route("/add", post(handler));
    let client = memory(router);

    let resp = client.post("/add", r#"{"x":3,"y":7}"#).await;
    assert_eq!(resp.body_str(), "sum=10");
}

// ═══════════════════════════════════════════════════════
// Synchronous handlers via sync_handler / sync_handler_with_req
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn sync_handler_0_arg_returns_str() {
    fn handler() -> &'static str {
        "sync-hello"
    }

    let router = Router::new().route("/test", get(sync_handler(handler)));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "sync-hello");
}

#[tokio::test]
async fn sync_handler_0_arg_returns_string() {
    fn handler() -> String {
        format!("sync-{}", 42)
    }

    let router = Router::new().route("/test", get(sync_handler(handler)));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "sync-42");
}

#[tokio::test]
async fn sync_handler_0_arg_returns_tuple() {
    fn handler() -> (u16, &'static str) {
        (201, "created-sync")
    }

    let router = Router::new().route("/test", post(sync_handler(handler)));
    let client = memory(router);
    let resp = client.post("/test", "").await;
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created-sync");
}

#[tokio::test]
async fn sync_handler_0_arg_returns_response() {
    fn handler() -> Response {
        Response::with_status(204)
    }

    let router = Router::new().route("/test", get(sync_handler(handler)));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.status, 204);
}

#[tokio::test]
async fn sync_handler_1_arg_echoes_body() {
    fn handler(req: Request) -> Response {
        Response::ok().with_body(req.body.clone())
    }

    let router = Router::new().route("/echo", post(sync_handler_with_req(handler)));
    let client = memory(router);
    let resp = client.post("/echo", "sync-data").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "sync-data");
}

#[tokio::test]
async fn sync_handler_1_arg_reads_path_param() {
    fn handler(req: Request) -> String {
        format!("user:{}", req.path_param("id").unwrap_or("?"))
    }

    let router = Router::new().route("/users/:id", get(sync_handler_with_req(handler)));
    let client = memory(router);
    let resp = client.get("/users/77").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "user:77");
}

#[tokio::test]
async fn sync_handler_1_arg_reads_query() {
    fn handler(req: Request) -> String {
        req.query_param("q").unwrap_or_default()
    }

    let router = Router::new().route("/search", get(sync_handler_with_req(handler)));
    let client = memory(router);
    let resp = client.get("/search?q=sync-search").await;
    assert_eq!(resp.body_str(), "sync-search");
}

#[tokio::test]
async fn sync_handler_returns_json() {
    #[derive(serde::Serialize)]
    struct Info {
        version: &'static str,
    }

    fn handler() -> Json<Info> {
        Json(Info { version: "1.0" })
    }

    let router = Router::new().route("/info", get(sync_handler(handler)));
    let client = memory(router);
    let resp = client.get("/info").await;
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["version"], "1.0");
}

// ═══════════════════════════════════════════════════════
// Mixed sync + async handlers on the same router
// ═══════════════════════════════════════════════════════

#[tokio::test]
async fn mixed_sync_and_async_handlers() {
    fn sync_health() -> &'static str {
        "sync-ok"
    }

    async fn async_health() -> &'static str {
        "async-ok"
    }

    fn sync_echo(req: Request) -> String {
        format!("sync:{}", req.body.len())
    }

    async fn async_echo(req: Request) -> String {
        format!("async:{}", req.body.len())
    }

    let router = Router::new()
        .route("/sync/health", get(sync_handler(sync_health)))
        .route("/async/health", get(async_health))
        .route("/sync/echo", post(sync_handler_with_req(sync_echo)))
        .route("/async/echo", post(async_echo));

    let client = memory(router);

    let resp = client.get("/sync/health").await;
    assert_eq!(resp.body_str(), "sync-ok");

    let resp = client.get("/async/health").await;
    assert_eq!(resp.body_str(), "async-ok");

    let resp = client.post("/sync/echo", "hello").await;
    assert_eq!(resp.body_str(), "sync:5");

    let resp = client.post("/async/echo", "hello").await;
    assert_eq!(resp.body_str(), "async:5");
}
