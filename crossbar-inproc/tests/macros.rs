use crossbar_inproc::handler;
use crossbar_inproc::prelude::*;

// ── Helper ────────────────────────────────────────────

fn inproc(router: Router) -> InProcessClient {
    InProcessClient::new(router)
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[path] extractor
// ═══════════════════════════════════════════════════════

#[handler]
fn get_by_symbol(#[path("symbol")] symbol: String) -> String {
    format!("symbol={symbol}")
}

#[test]
fn handler_path_extractor() {
    let router = Router::new().route("/asset/:symbol", get(get_by_symbol));
    let client = inproc(router);
    let resp = client.get("/asset/BTCUSD");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "symbol=BTCUSD");
}

#[test]
fn handler_path_extractor_missing_returns_400() {
    // Route has no :symbol param, so path_param returns None -> 400
    let router = Router::new().route("/plain", get(get_by_symbol));
    let client = inproc(router);
    let resp = client.get("/plain");
    assert_eq!(resp.status, 400);
    assert!(resp.body_str().contains("missing path param"));
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[query] extractor
// ═══════════════════════════════════════════════════════

#[handler]
fn search(#[query("q")] q: String, #[query("limit")] limit: Option<String>) -> String {
    let limit_str = limit.unwrap_or_else(|| "10".into());
    format!("q={q}&limit={limit_str}")
}

#[test]
fn handler_query_extractors() {
    let router = Router::new().route("/search", get(search));
    let client = inproc(router);

    let resp = client.get("/search?q=rust&limit=5");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=rust&limit=5");
}

#[test]
fn handler_query_optional_missing() {
    let router = Router::new().route("/search", get(search));
    let client = inproc(router);

    let resp = client.get("/search?q=rust");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=rust&limit=10");
}

#[test]
fn handler_query_required_missing_returns_400() {
    let router = Router::new().route("/search", get(search));
    let client = inproc(router);

    let resp = client.get("/search");
    assert_eq!(resp.status, 400);
    assert!(resp.body_str().contains("missing query param"));
}

// ═══════════════════════════════════════════════════════
// #[handler] with Request passthrough (no attribute)
// ═══════════════════════════════════════════════════════

#[handler]
fn passthrough(req: Request) -> String {
    format!("method={}", req.method)
}

#[test]
fn handler_request_passthrough() {
    let router = Router::new().route("/info", get(passthrough));
    let client = inproc(router);

    let resp = client.get("/info");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "method=GET");
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[path] on Option<String>
// ═══════════════════════════════════════════════════════

#[handler]
fn optional_path(#[path("id")] id: Option<String>) -> String {
    id.unwrap_or_else(|| "none".into())
}

#[test]
fn handler_optional_path_present() {
    let router = Router::new().route("/items/:id", get(optional_path));
    let client = inproc(router);
    let resp = client.get("/items/42");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "42");
}

#[test]
fn handler_optional_path_missing() {
    // No :id in the pattern, so path_param returns None -> Option is None
    let router = Router::new().route("/items", get(optional_path));
    let client = inproc(router);
    let resp = client.get("/items");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "none");
}
