use crossbar::prelude::*;

// ── Helpers ────────────────────────────────────────────

fn test_router() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/items",
            get(|| async { Response::json(&vec!["a", "b", "c"]) }),
        )
        .route(
            "/items/:id",
            get(|req: Request| async move {
                let id = req.path_param("id").unwrap_or("?").to_string();
                format!("item:{id}")
            }),
        )
        .route(
            "/items",
            post(|req: Request| async move {
                let body = std::str::from_utf8(&req.body).unwrap_or("").to_string();
                (201u16, format!("created:{body}"))
            }),
        )
        .route(
            "/market/:exchange/:symbol",
            get(|req: Request| async move {
                let exchange = req.path_param("exchange").unwrap_or("?").to_string();
                let symbol = req.path_param("symbol").unwrap_or("?").to_string();
                format!("{exchange}:{symbol}")
            }),
        )
        .route(
            "/search",
            get(|req: Request| async move {
                let q = req.query_param("q").unwrap_or_default();
                let page = req.query_param("page").unwrap_or_default();
                format!("q={q}&page={page}")
            }),
        )
        .route(
            "/echo",
            put(|req: Request| async move { Response::ok().with_body(req.body.clone()) }),
        )
        .route("/echo", delete(|| async { (200u16, "deleted") }))
        .route("/echo", patch(|| async { (200u16, "patched") }))
}

fn memory(router: Router) -> MemoryClient {
    MemoryClient::new(router)
}

// ── Basic route matching ───────────────────────────────

#[tokio::test]
async fn basic_route_exact_match_200() {
    let client = memory(test_router());
    let resp = client.get("/health").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[tokio::test]
async fn basic_route_items_get() {
    let client = memory(test_router());
    let resp = client.get("/items").await;
    assert_eq!(resp.status, 200);
    // Should be JSON array
    let body: Vec<String> = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body, vec!["a", "b", "c"]);
}

// ── Path parameters ────────────────────────────────────

#[tokio::test]
async fn path_param_single() {
    let client = memory(test_router());
    let resp = client.get("/items/42").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:42");
}

#[tokio::test]
async fn path_param_string_value() {
    let client = memory(test_router());
    let resp = client.get("/items/hello-world").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:hello-world");
}

#[tokio::test]
async fn multiple_path_params() {
    let client = memory(test_router());
    let resp = client.get("/market/binance/BTCUSDT").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "binance:BTCUSDT");
}

// ── Query parameters ───────────────────────────────────

#[tokio::test]
async fn query_params_extraction() {
    let client = memory(test_router());
    let resp = client.get("/search?q=rust&page=3").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=rust&page=3");
}

#[tokio::test]
async fn query_params_missing_values() {
    let client = memory(test_router());
    let resp = client.get("/search").await;
    assert_eq!(resp.status, 200);
    // Both q and page should be empty strings via unwrap_or_default
    assert_eq!(resp.body_str(), "q=&page=");
}

#[tokio::test]
async fn query_params_partial() {
    let client = memory(test_router());
    let resp = client.get("/search?q=hello").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=hello&page=");
}

// ── Percent-encoded parameters ─────────────────────────

#[tokio::test]
async fn percent_encoded_path_param() {
    let client = memory(test_router());
    // %20 -> space in path param
    let resp = client.get("/items/hello%20world").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:hello world");
}

#[tokio::test]
async fn percent_encoded_slash_in_path_param() {
    // %2F in a segment won't match because path splitting happens before decoding
    // The router splits on '/', so %2F stays as one segment and is decoded to '/'
    let client = memory(test_router());
    let resp = client.get("/items/a%2Fb").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:a/b");
}

#[tokio::test]
async fn percent_encoded_query_params() {
    let client = memory(test_router());
    let resp = client.get("/search?q=hello%20world&page=1").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=hello world&page=1");
}

#[tokio::test]
async fn plus_as_space_in_query() {
    let client = memory(test_router());
    let resp = client.get("/search?q=hello+world&page=1").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=hello world&page=1");
}

// ── 404 for unmatched routes ───────────────────────────

#[tokio::test]
async fn unmatched_route_returns_404() {
    let client = memory(test_router());
    let resp = client.get("/nonexistent").await;
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body_str(), "not found");
}

#[tokio::test]
async fn unmatched_deep_path_404() {
    let client = memory(test_router());
    let resp = client.get("/a/b/c/d/e").await;
    assert_eq!(resp.status, 404);
}

// ── Method matching ────────────────────────────────────

#[tokio::test]
async fn get_handler_does_not_match_post() {
    let client = memory(test_router());
    // /health is only registered for GET
    let resp = client.post("/health", "body").await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn post_handler_matches_post() {
    let client = memory(test_router());
    let resp = client.post("/items", "new-item").await;
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created:new-item");
}

// ── Multiple routes same path different methods ────────

#[tokio::test]
async fn same_path_different_methods() {
    let client = memory(test_router());

    // PUT /echo
    let resp = client
        .request(Request::new(Method::Put, "/echo").with_body("hello"))
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello");

    // DELETE /echo
    let resp = client.request(Request::new(Method::Delete, "/echo")).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "deleted");

    // PATCH /echo
    let resp = client.request(Request::new(Method::Patch, "/echo")).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "patched");

    // GET /echo -> 404 (not registered)
    let resp = client.get("/echo").await;
    assert_eq!(resp.status, 404);
}

// ── Route ordering (first match wins) ──────────────────

#[tokio::test]
async fn first_match_wins() {
    let router = Router::new()
        .route("/test", get(|| async { "first" }))
        .route("/test", get(|| async { "second" }));
    let client = memory(router);
    let resp = client.get("/test").await;
    assert_eq!(resp.body_str(), "first");
}

// ── Trailing slashes ───────────────────────────────────

#[tokio::test]
async fn trailing_slash_matches() {
    // The router filters empty segments, so "/health/" and "/health" should match the same
    let client = memory(test_router());
    let resp = client.get("/health/").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[tokio::test]
async fn leading_and_trailing_slashes() {
    let client = memory(test_router());
    let resp = client.get("///health///").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

// ── Empty path segments ────────────────────────────────

#[tokio::test]
async fn double_slash_in_path() {
    // /items//42 -> segments ["items", "42"] because empty segments are filtered
    let client = memory(test_router());
    let resp = client.get("/items//42").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:42");
}

// ── Unicode in path params ─────────────────────────────

#[tokio::test]
async fn percent_encoded_unicode_path_param() {
    // Use percent-encoded UTF-8 bytes for unicode characters.
    // "café" = 63 61 66 c3 a9 => "caf%C3%A9"
    let client = memory(test_router());
    let resp = client.get("/items/caf%C3%A9").await;
    assert_eq!(resp.status, 200);
    // percent_decode accumulates raw bytes and converts via String::from_utf8,
    // so multi-byte UTF-8 sequences are decoded correctly.
    assert_eq!(resp.body_str(), "item:café");
}

#[tokio::test]
async fn ascii_path_param_unicode_safe() {
    // ASCII path params work fine
    let client = memory(test_router());
    let resp = client.get("/items/hello-world").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:hello-world");
}

// ── Router::routes_info ────────────────────────────────

#[tokio::test]
async fn routes_info_returns_all_routes() {
    let router = test_router();
    let info = router.routes_info();

    // We registered 9 routes in test_router
    assert_eq!(info.len(), 9);

    // Check a few specific routes
    assert!(info.contains(&(Method::Get, "/health".to_string())));
    assert!(info.contains(&(Method::Get, "/items".to_string())));
    assert!(info.contains(&(Method::Get, "/items/:id".to_string())));
    assert!(info.contains(&(Method::Post, "/items".to_string())));
    assert!(info.contains(&(Method::Get, "/market/:exchange/:symbol".to_string())));
    assert!(info.contains(&(Method::Get, "/search".to_string())));
    assert!(info.contains(&(Method::Put, "/echo".to_string())));
    assert!(info.contains(&(Method::Delete, "/echo".to_string())));
    assert!(info.contains(&(Method::Patch, "/echo".to_string())));
}

// ── Router clone shares routes ─────────────────────────

#[tokio::test]
async fn router_clone_shares_routes() {
    let router = test_router();
    let clone = router.clone();

    // Both should have the same routes
    assert_eq!(router.routes_info().len(), clone.routes_info().len());
    assert_eq!(router.routes_info(), clone.routes_info());

    // Both should work independently
    let client1 = memory(router);
    let client2 = memory(clone);

    let r1 = client1.get("/health").await;
    let r2 = client2.get("/health").await;
    assert_eq!(r1.status, r2.status);
    assert_eq!(r1.body_str(), r2.body_str());
}

// ── Edge cases ─────────────────────────────────────────

#[tokio::test]
async fn root_path_route() {
    let router = Router::new().route("/", get(|| async { "root" }));
    let client = memory(router);
    let resp = client.get("/").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "root");
}

#[tokio::test]
async fn empty_router_returns_404() {
    let router = Router::new();
    let client = memory(router);
    let resp = client.get("/anything").await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn path_param_with_dots() {
    let router = Router::new().route(
        "/files/:filename",
        get(|req: Request| async move { req.path_param("filename").unwrap_or("?").to_string() }),
    );
    let client = memory(router);
    let resp = client.get("/files/document.pdf").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "document.pdf");
}

#[tokio::test]
async fn path_param_with_special_chars() {
    let router = Router::new().route(
        "/users/:name",
        get(|req: Request| async move { req.path_param("name").unwrap_or("?").to_string() }),
    );
    let client = memory(router);
    let resp = client.get("/users/john-doe_123").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "john-doe_123");
}

#[tokio::test]
async fn query_with_empty_value() {
    let router = Router::new().route(
        "/test",
        get(|req: Request| async move {
            let params = req.query_params();
            format!(
                "key={}",
                params.get("key").map(|s| s.as_str()).unwrap_or("MISSING")
            )
        }),
    );
    let client = memory(router);
    let resp = client.get("/test?key=").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "key=");
}

#[tokio::test]
async fn query_with_no_equals() {
    let router = Router::new().route(
        "/test",
        get(|req: Request| async move {
            let params = req.query_params();
            format!(
                "flag={}",
                params.get("flag").map(|s| s.as_str()).unwrap_or("MISSING")
            )
        }),
    );
    let client = memory(router);
    // "flag" with no '=' => value should be ""
    let resp = client.get("/test?flag").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "flag=");
}
