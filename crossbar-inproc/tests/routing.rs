use crossbar_inproc::prelude::*;

// ── Helpers ────────────────────────────────────────────

fn test_router() -> Router {
    let router = Router::new()
        .route("/health", get(|| "ok"))
        .route(
            "/items/:id",
            get(|req: Request| {
                let id = req.path_param("id").unwrap_or("?").to_string();
                format!("item:{id}")
            }),
        )
        .route(
            "/items",
            post(|req: Request| {
                let body = std::str::from_utf8(&req.body).unwrap_or("").to_string();
                (201u16, format!("created:{body}"))
            }),
        )
        .route(
            "/market/:exchange/:symbol",
            get(|req: Request| {
                let exchange = req.path_param("exchange").unwrap_or("?").to_string();
                let symbol = req.path_param("symbol").unwrap_or("?").to_string();
                format!("{exchange}:{symbol}")
            }),
        )
        .route(
            "/search",
            get(|req: Request| {
                let q = req.query_param("q").unwrap_or_default();
                let page = req.query_param("page").unwrap_or_default();
                format!("q={q}&page={page}")
            }),
        )
        .route(
            "/echo",
            put(|req: Request| Response::ok().with_body(req.body.clone())),
        )
        .route("/echo", delete(|| (200u16, "deleted")))
        .route("/echo", patch(|| (200u16, "patched")));

    router
}

fn inproc(router: Router) -> InProcessClient {
    InProcessClient::new(router)
}

// ── Basic route matching ───────────────────────────────

#[test]
fn basic_route_exact_match_200() {
    let client = inproc(test_router());
    let resp = client.get("/health");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

// ── Path parameters ────────────────────────────────────

#[test]
fn path_param_single() {
    let client = inproc(test_router());
    let resp = client.get("/items/42");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:42");
}

#[test]
fn path_param_string_value() {
    let client = inproc(test_router());
    let resp = client.get("/items/hello-world");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:hello-world");
}

#[test]
fn multiple_path_params() {
    let client = inproc(test_router());
    let resp = client.get("/market/binance/BTCUSDT");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "binance:BTCUSDT");
}

// ── Query parameters ───────────────────────────────────

#[test]
fn query_params_extraction() {
    let client = inproc(test_router());
    let resp = client.get("/search?q=rust&page=3");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=rust&page=3");
}

#[test]
fn query_params_missing_values() {
    let client = inproc(test_router());
    let resp = client.get("/search");
    assert_eq!(resp.status, 200);
    // Both q and page should be empty strings via unwrap_or_default
    assert_eq!(resp.body_str(), "q=&page=");
}

#[test]
fn query_params_partial() {
    let client = inproc(test_router());
    let resp = client.get("/search?q=hello");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=hello&page=");
}

// ── Percent-encoded parameters ─────────────────────────

#[test]
fn percent_encoded_path_param() {
    let client = inproc(test_router());
    // %20 -> space in path param
    let resp = client.get("/items/hello%20world");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:hello world");
}

#[test]
fn percent_encoded_slash_in_path_param() {
    // %2F in a segment won't match because path splitting happens before decoding
    // The router splits on '/', so %2F stays as one segment and is decoded to '/'
    let client = inproc(test_router());
    let resp = client.get("/items/a%2Fb");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:a/b");
}

#[test]
fn percent_encoded_query_params() {
    let client = inproc(test_router());
    let resp = client.get("/search?q=hello%20world&page=1");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=hello world&page=1");
}

#[test]
fn plus_as_space_in_query() {
    let client = inproc(test_router());
    let resp = client.get("/search?q=hello+world&page=1");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=hello world&page=1");
}

// ── 404 for unmatched routes ───────────────────────────

#[test]
fn unmatched_route_returns_404() {
    let client = inproc(test_router());
    let resp = client.get("/nonexistent");
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body_str(), "not found");
}

#[test]
fn unmatched_deep_path_404() {
    let client = inproc(test_router());
    let resp = client.get("/a/b/c/d/e");
    assert_eq!(resp.status, 404);
}

// ── Method matching ────────────────────────────────────

#[test]
fn get_handler_does_not_match_post() {
    let client = inproc(test_router());
    // /health is only registered for GET
    let resp = client.post("/health", "body");
    assert_eq!(resp.status, 404);
}

#[test]
fn post_handler_matches_post() {
    let client = inproc(test_router());
    let resp = client.post("/items", "new-item");
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created:new-item");
}

// ── Multiple routes same path different methods ────────

#[test]
fn same_path_different_methods() {
    let client = inproc(test_router());

    // PUT /echo
    let resp = client.request(Request::new(Method::Put, "/echo").with_body("hello"));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello");

    // DELETE /echo
    let resp = client.request(Request::new(Method::Delete, "/echo"));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "deleted");

    // PATCH /echo
    let resp = client.request(Request::new(Method::Patch, "/echo"));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "patched");

    // GET /echo -> 404 (not registered)
    let resp = client.get("/echo");
    assert_eq!(resp.status, 404);
}

// ── Route ordering (first match wins) ──────────────────

#[test]
fn first_match_wins() {
    let router = Router::new()
        .route("/test", get(|| "first"))
        .route("/test", get(|| "second"));
    let client = inproc(router);
    let resp = client.get("/test");
    assert_eq!(resp.body_str(), "first");
}

// ── Trailing slashes ───────────────────────────────────

#[test]
fn trailing_slash_matches() {
    // The router filters empty segments, so "/health/" and "/health" should match the same
    let client = inproc(test_router());
    let resp = client.get("/health/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[test]
fn leading_and_trailing_slashes() {
    let client = inproc(test_router());
    let resp = client.get("///health///");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

// ── Empty path segments ────────────────────────────────

#[test]
fn double_slash_in_path() {
    // /items//42 -> segments ["items", "42"] because empty segments are filtered
    let client = inproc(test_router());
    let resp = client.get("/items//42");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:42");
}

// ── Unicode in path params ─────────────────────────────

#[test]
fn percent_encoded_unicode_path_param() {
    // Use percent-encoded UTF-8 bytes for unicode characters.
    // "café" = 63 61 66 c3 a9 => "caf%C3%A9"
    let client = inproc(test_router());
    let resp = client.get("/items/caf%C3%A9");
    assert_eq!(resp.status, 200);
    // percent_decode accumulates raw bytes and converts via String::from_utf8,
    // so multi-byte UTF-8 sequences are decoded correctly.
    assert_eq!(resp.body_str(), "item:café");
}

#[test]
fn ascii_path_param_unicode_safe() {
    // ASCII path params work fine
    let client = inproc(test_router());
    let resp = client.get("/items/hello-world");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "item:hello-world");
}

// ── Router::routes_info ────────────────────────────────

#[test]
fn routes_info_returns_all_routes() {
    let router = test_router();
    let info = router.routes_info();

    // We registered 8 routes in test_router
    assert_eq!(info.len(), 8);

    // Check a few specific routes
    assert!(info.contains(&(Method::Get, "/health".to_string())));
    assert!(info.contains(&(Method::Get, "/items/:id".to_string())));
    assert!(info.contains(&(Method::Post, "/items".to_string())));
    assert!(info.contains(&(Method::Get, "/market/:exchange/:symbol".to_string())));
    assert!(info.contains(&(Method::Get, "/search".to_string())));
    assert!(info.contains(&(Method::Put, "/echo".to_string())));
    assert!(info.contains(&(Method::Delete, "/echo".to_string())));
    assert!(info.contains(&(Method::Patch, "/echo".to_string())));
}

// ── Router clone shares routes ─────────────────────────

#[test]
fn router_clone_shares_routes() {
    let router = test_router();
    let clone = router.clone();

    // Both should have the same routes
    assert_eq!(router.routes_info().len(), clone.routes_info().len());
    assert_eq!(router.routes_info(), clone.routes_info());

    // Both should work independently
    let client1 = inproc(router);
    let client2 = inproc(clone);

    let r1 = client1.get("/health");
    let r2 = client2.get("/health");
    assert_eq!(r1.status, r2.status);
    assert_eq!(r1.body_str(), r2.body_str());
}

// ── Edge cases ─────────────────────────────────────────

#[test]
fn root_path_route() {
    let router = Router::new().route("/", get(|| "root"));
    let client = inproc(router);
    let resp = client.get("/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "root");
}

#[test]
fn empty_router_returns_404() {
    let router = Router::new();
    let client = inproc(router);
    let resp = client.get("/anything");
    assert_eq!(resp.status, 404);
}

#[test]
fn path_param_with_dots() {
    let router = Router::new().route(
        "/files/:filename",
        get(|req: Request| req.path_param("filename").unwrap_or("?").to_string()),
    );
    let client = inproc(router);
    let resp = client.get("/files/document.pdf");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "document.pdf");
}

#[test]
fn path_param_with_special_chars() {
    let router = Router::new().route(
        "/users/:name",
        get(|req: Request| req.path_param("name").unwrap_or("?").to_string()),
    );
    let client = inproc(router);
    let resp = client.get("/users/john-doe_123");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "john-doe_123");
}

#[test]
fn query_with_empty_value() {
    let router = Router::new().route(
        "/test",
        get(|req: Request| {
            let params = req.query_params();
            format!(
                "key={}",
                params.get("key").map(|s| s.as_str()).unwrap_or("MISSING")
            )
        }),
    );
    let client = inproc(router);
    let resp = client.get("/test?key=");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "key=");
}

#[test]
fn query_with_no_equals() {
    let router = Router::new().route(
        "/test",
        get(|req: Request| {
            let params = req.query_params();
            format!(
                "flag={}",
                params.get("flag").map(|s| s.as_str()).unwrap_or("MISSING")
            )
        }),
    );
    let client = inproc(router);
    // "flag" with no '=' => value should be ""
    let resp = client.get("/test?flag");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "flag=");
}
