use crossbar_inproc::prelude::*;

// ── Helpers ────────────────────────────────────────────

fn inproc(router: Router) -> InProcessClient {
    InProcessClient::new(router)
}

// ═══════════════════════════════════════════════════════
// 0-arg handler: fn() -> impl IntoResponse
// ═══════════════════════════════════════════════════════

#[test]
fn handler_0_arg_returns_str() {
    fn handler() -> &'static str {
        "hello"
    }

    let router = Router::new().route("/test", get(handler));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello");
}

#[test]
fn handler_0_arg_returns_string() {
    fn handler() -> String {
        String::from("dynamic")
    }

    let router = Router::new().route("/test", get(handler));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "dynamic");
}

#[test]
fn handler_0_arg_returns_response() {
    fn handler() -> Response {
        Response::with_status(201).with_body("created")
    }

    let router = Router::new().route("/test", post(handler));
    let client = inproc(router);
    let resp = client.post("/test", "");
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created");
}

#[test]
fn handler_0_arg_returns_tuple() {
    fn handler() -> (u16, &'static str) {
        (202, "accepted")
    }

    let router = Router::new().route("/test", get(handler));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.status, 202);
    assert_eq!(resp.body_str(), "accepted");
}

// ═══════════════════════════════════════════════════════
// 1-arg handler: fn(Request) -> impl IntoResponse
// ═══════════════════════════════════════════════════════

#[test]
fn handler_1_arg_echoes_body() {
    fn handler(req: Request) -> Response {
        Response::ok().with_body(req.body.clone())
    }

    let router = Router::new().route("/echo", post(handler));
    let client = inproc(router);
    let resp = client.post("/echo", "test data");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "test data");
}

#[test]
fn handler_1_arg_reads_path_param() {
    fn handler(req: Request) -> String {
        format!("user:{}", req.path_param("id").unwrap_or("?"))
    }

    let router = Router::new().route("/users/:id", get(handler));
    let client = inproc(router);
    let resp = client.get("/users/99");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "user:99");
}

#[test]
fn handler_1_arg_reads_query() {
    fn handler(req: Request) -> String {
        req.query_param("q").unwrap_or_default()
    }

    let router = Router::new().route("/search", get(handler));
    let client = inproc(router);
    let resp = client.get("/search?q=hello");
    assert_eq!(resp.body_str(), "hello");
}

// ═══════════════════════════════════════════════════════
// Handler returning JSON via manual serialization
// ═══════════════════════════════════════════════════════

#[test]
fn handler_returns_json_manual() {
    #[derive(serde::Serialize)]
    struct User {
        name: String,
        age: u32,
    }

    fn handler() -> Response {
        let user = User {
            name: "Alice".into(),
            age: 30,
        };
        let bytes = serde_json::to_vec(&user).unwrap();
        Response::ok()
            .with_body(bytes)
            .with_header("content-type", "application/json")
    }

    let router = Router::new().route("/user", get(handler));
    let client = inproc(router);
    let resp = client.get("/user");
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["name"], "Alice");
    assert_eq!(v["age"], 30);
}

#[test]
fn handler_returns_json_array_manual() {
    fn handler() -> Vec<u8> {
        serde_json::to_vec(&vec![1, 2, 3]).unwrap()
    }

    let router = Router::new().route("/nums", get(handler));
    let client = inproc(router);
    let resp = client.get("/nums");
    let v: Vec<i32> = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v, vec![1, 2, 3]);
}

// ═══════════════════════════════════════════════════════
// Handler returning Result<Ok, Err>
// ═══════════════════════════════════════════════════════

#[test]
fn handler_returns_result_ok() {
    fn handler(req: Request) -> Result<String, (u16, &'static str)> {
        let id = req.path_param("id").unwrap_or("0");
        Ok(format!("found:{id}"))
    }

    let router = Router::new().route("/items/:id", get(handler));
    let client = inproc(router);
    let resp = client.get("/items/5");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "found:5");
}

#[test]
fn handler_returns_result_err() {
    fn handler(_req: Request) -> Result<&'static str, (u16, &'static str)> {
        Err((400, "bad input"))
    }

    let router = Router::new().route("/fail", get(handler));
    let client = inproc(router);
    let resp = client.get("/fail");
    assert_eq!(resp.status, 400);
    assert_eq!(resp.body_str(), "bad input");
}

// ═══════════════════════════════════════════════════════
// Handler returning (u16, &str) tuple
// ═══════════════════════════════════════════════════════

#[test]
fn handler_returns_status_tuple() {
    fn handler() -> (u16, &'static str) {
        (418, "I'm a teapot")
    }

    let router = Router::new().route("/teapot", get(handler));
    let client = inproc(router);
    let resp = client.get("/teapot");
    assert_eq!(resp.status, 418);
    assert_eq!(resp.body_str(), "I'm a teapot");
}

#[test]
fn handler_returns_status_tuple_string() {
    fn handler() -> (u16, String) {
        (503, "service unavailable".to_string())
    }

    let router = Router::new().route("/status", get(handler));
    let client = inproc(router);
    let resp = client.get("/status");
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body_str(), "service unavailable");
}

// ═══════════════════════════════════════════════════════
// Handler returning String
// ═══════════════════════════════════════════════════════

#[test]
fn handler_returns_owned_string() {
    fn handler() -> String {
        format!("computed: {}", 42)
    }

    let router = Router::new().route("/computed", get(handler));
    let client = inproc(router);
    let resp = client.get("/computed");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "computed: 42");
}

// ═══════════════════════════════════════════════════════
// BoxedHandler clone and call (tested via Router since call is pub(crate))
// ═══════════════════════════════════════════════════════

#[test]
fn boxed_handler_clone_via_router() {
    // BoxedHandler is Clone (uses Arc internally). Test that a cloned router
    // (which clones the internal Arc<Vec<Route>>) still dispatches correctly.
    let router = Router::new().route("/test", get(|| "cloned"));
    let cloned = router.clone();

    let client = inproc(cloned);
    let resp = client.get("/test");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "cloned");
}

#[test]
fn boxed_handler_multiple_calls_via_router() {
    let router = Router::new().route("/test", get(|| "multi"));
    let client = inproc(router);

    for _ in 0..5 {
        let resp = client.get("/test");
        assert_eq!(resp.body_str(), "multi");
    }
}

// ═══════════════════════════════════════════════════════
// Closure handlers (not just fn items)
// ═══════════════════════════════════════════════════════

#[test]
fn closure_handler_0_arg() {
    let router = Router::new().route("/test", get(|| "closure"));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.body_str(), "closure");
}

#[test]
fn closure_handler_1_arg() {
    let router = Router::new().route(
        "/echo",
        post(|req: Request| Response::ok().with_body(req.body.clone())),
    );
    let client = inproc(router);
    let resp = client.post("/echo", "data");
    assert_eq!(resp.body_str(), "data");
}

// ═══════════════════════════════════════════════════════
// Handler with JSON body deserialization (manual)
// ═══════════════════════════════════════════════════════

#[test]
fn handler_deserializes_json_body_manual() {
    #[derive(serde::Deserialize)]
    struct Input {
        x: i32,
        y: i32,
    }

    fn handler(req: Request) -> String {
        match serde_json::from_slice::<Input>(&req.body) {
            Ok(input) => format!("sum={}", input.x + input.y),
            Err(e) => format!("error: {e}"),
        }
    }

    let router = Router::new().route("/add", post(handler));
    let client = inproc(router);

    let resp = client.post("/add", r#"{"x":3,"y":7}"#);
    assert_eq!(resp.body_str(), "sum=10");
}

// ═══════════════════════════════════════════════════════
// Handlers: fn items and closures
// ═══════════════════════════════════════════════════════

#[test]
fn handler_fn_0_arg_returns_str() {
    fn handler() -> &'static str {
        "sync-hello"
    }

    let router = Router::new().route("/test", get(handler));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "sync-hello");
}

#[test]
fn handler_fn_0_arg_returns_string() {
    fn handler() -> String {
        format!("sync-{}", 42)
    }

    let router = Router::new().route("/test", get(handler));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "sync-42");
}

#[test]
fn handler_fn_0_arg_returns_tuple() {
    fn handler() -> (u16, &'static str) {
        (201, "created-sync")
    }

    let router = Router::new().route("/test", post(handler));
    let client = inproc(router);
    let resp = client.post("/test", "");
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created-sync");
}

#[test]
fn handler_fn_0_arg_returns_response() {
    fn handler() -> Response {
        Response::with_status(204)
    }

    let router = Router::new().route("/test", get(handler));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.status, 204);
}

#[test]
fn handler_fn_1_arg_echoes_body() {
    fn handler(req: Request) -> Response {
        Response::ok().with_body(req.body.clone())
    }

    let router = Router::new().route("/echo", post(handler));
    let client = inproc(router);
    let resp = client.post("/echo", "sync-data");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "sync-data");
}

#[test]
fn handler_fn_1_arg_reads_path_param() {
    fn handler(req: Request) -> String {
        format!("user:{}", req.path_param("id").unwrap_or("?"))
    }

    let router = Router::new().route("/users/:id", get(handler));
    let client = inproc(router);
    let resp = client.get("/users/77");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "user:77");
}

#[test]
fn handler_fn_1_arg_reads_query() {
    fn handler(req: Request) -> String {
        req.query_param("q").unwrap_or_default()
    }

    let router = Router::new().route("/search", get(handler));
    let client = inproc(router);
    let resp = client.get("/search?q=sync-search");
    assert_eq!(resp.body_str(), "sync-search");
}

#[test]
fn handler_fn_returns_json_manual() {
    #[derive(serde::Serialize)]
    struct Info {
        version: &'static str,
    }

    fn handler() -> Response {
        let bytes = serde_json::to_vec(&Info { version: "1.0" }).unwrap();
        Response::ok()
            .with_body(bytes)
            .with_header("content-type", "application/json")
    }

    let router = Router::new().route("/info", get(handler));
    let client = inproc(router);
    let resp = client.get("/info");
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["version"], "1.0");
}

// ═══════════════════════════════════════════════════════
// Multiple handlers on the same router
// ═══════════════════════════════════════════════════════

#[test]
fn multiple_handlers_on_same_router() {
    fn health() -> &'static str {
        "ok"
    }

    fn echo(req: Request) -> String {
        format!("len:{}", req.body.len())
    }

    let router = Router::new()
        .route("/health", get(health))
        .route("/echo", post(echo));

    let client = inproc(router);

    let resp = client.get("/health");
    assert_eq!(resp.body_str(), "ok");

    let resp = client.post("/echo", "hello");
    assert_eq!(resp.body_str(), "len:5");
}
