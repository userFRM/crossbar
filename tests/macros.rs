use crossbar::prelude::*;
use serde::{Deserialize, Serialize};

// ── Helper ────────────────────────────────────────────

fn memory(router: Router) -> MemoryClient {
    MemoryClient::new(router)
}

// ═══════════════════════════════════════════════════════
// #[derive(IntoResponse)]
// ═══════════════════════════════════════════════════════

#[derive(Serialize, IntoResponse)]
struct OhlcData {
    symbol: String,
    open: f64,
    close: f64,
}

#[tokio::test]
async fn derive_into_response_json() {
    async fn ohlc() -> OhlcData {
        OhlcData {
            symbol: "AAPL".into(),
            open: 150.0,
            close: 155.0,
        }
    }

    let router = Router::new().route("/ohlc", get(ohlc));
    let client = memory(router);
    let resp = client.get("/ohlc").await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );

    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["symbol"], "AAPL");
    assert_eq!(v["open"], 150.0);
    assert_eq!(v["close"], 155.0);
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[path] extractor
// ═══════════════════════════════════════════════════════

#[handler]
async fn get_by_symbol(#[path("symbol")] symbol: String) -> String {
    format!("symbol={symbol}")
}

#[tokio::test]
async fn handler_path_extractor() {
    let router = Router::new().route("/asset/:symbol", get(get_by_symbol));
    let client = memory(router);
    let resp = client.get("/asset/BTCUSD").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "symbol=BTCUSD");
}

#[tokio::test]
async fn handler_path_extractor_missing_returns_400() {
    // Route has no :symbol param, so path_param returns None -> 400
    let router = Router::new().route("/plain", get(get_by_symbol));
    let client = memory(router);
    let resp = client.get("/plain").await;
    assert_eq!(resp.status, 400);
    assert!(resp.body_str().contains("missing path param"));
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[query] extractor
// ═══════════════════════════════════════════════════════

#[handler]
async fn search(#[query("q")] q: String, #[query("limit")] limit: Option<String>) -> String {
    let limit_str = limit.unwrap_or_else(|| "10".into());
    format!("q={q}&limit={limit_str}")
}

#[tokio::test]
async fn handler_query_extractors() {
    let router = Router::new().route("/search", get(search));
    let client = memory(router);

    let resp = client.get("/search?q=rust&limit=5").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=rust&limit=5");
}

#[tokio::test]
async fn handler_query_optional_missing() {
    let router = Router::new().route("/search", get(search));
    let client = memory(router);

    let resp = client.get("/search?q=rust").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "q=rust&limit=10");
}

#[tokio::test]
async fn handler_query_required_missing_returns_400() {
    let router = Router::new().route("/search", get(search));
    let client = memory(router);

    let resp = client.get("/search").await;
    assert_eq!(resp.status, 400);
    assert!(resp.body_str().contains("missing query param"));
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[body] extractor
// ═══════════════════════════════════════════════════════

#[derive(Deserialize)]
struct CreateOrder {
    symbol: String,
    quantity: u32,
}

#[handler]
async fn create_order(#[body] order: CreateOrder) -> String {
    format!("order: {} x{}", order.symbol, order.quantity)
}

#[tokio::test]
async fn handler_body_extractor() {
    let router = Router::new().route("/orders", post(create_order));
    let client = memory(router);

    let resp = client
        .post("/orders", r#"{"symbol":"AAPL","quantity":100}"#)
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "order: AAPL x100");
}

#[tokio::test]
async fn handler_body_extractor_invalid_json_returns_400() {
    let router = Router::new().route("/orders", post(create_order));
    let client = memory(router);

    let resp = client.post("/orders", "not json").await;
    assert_eq!(resp.status, 400);
    assert!(resp.body_str().contains("invalid body"));
}

// ═══════════════════════════════════════════════════════
// #[handler] with mixed extractors
// ═══════════════════════════════════════════════════════

#[derive(Deserialize)]
struct UpdatePayload {
    price: f64,
}

#[handler]
async fn update_asset(
    #[path("symbol")] symbol: String,
    #[query("venue")] venue: Option<String>,
    #[body] payload: UpdatePayload,
) -> String {
    let venue_str = venue.unwrap_or_else(|| "default".into());
    format!(
        "update {} on {}: price={}",
        symbol, venue_str, payload.price
    )
}

#[tokio::test]
async fn handler_mixed_extractors() {
    let router = Router::new().route("/assets/:symbol", post(update_asset));
    let client = memory(router);

    let resp = client
        .post("/assets/ETHUSD?venue=binance", r#"{"price":3200.50}"#)
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "update ETHUSD on binance: price=3200.5");
}

// ═══════════════════════════════════════════════════════
// #[handler] with Request passthrough (no attribute)
// ═══════════════════════════════════════════════════════

#[handler]
async fn passthrough(req: Request) -> String {
    format!("method={}", req.method)
}

#[tokio::test]
async fn handler_request_passthrough() {
    let router = Router::new().route("/info", get(passthrough));
    let client = memory(router);

    let resp = client.get("/info").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "method=GET");
}

// ═══════════════════════════════════════════════════════
// #[handler] + #[derive(IntoResponse)] end-to-end
// ═══════════════════════════════════════════════════════

#[derive(Serialize, IntoResponse)]
struct AssetInfo {
    symbol: String,
    venue: String,
}

#[handler]
async fn get_asset_info(
    #[path("symbol")] symbol: String,
    #[query("venue")] venue: Option<String>,
) -> AssetInfo {
    AssetInfo {
        symbol,
        venue: venue.unwrap_or_else(|| "unknown".into()),
    }
}

#[tokio::test]
async fn handler_with_derive_into_response_end_to_end() {
    let router = Router::new().route("/assets/:symbol", get(get_asset_info));
    let client = memory(router);

    let resp = client.get("/assets/SOLUSD?venue=ftx").await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );

    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["symbol"], "SOLUSD");
    assert_eq!(v["venue"], "ftx");
}

// ═══════════════════════════════════════════════════════
// #[handler] with #[path] on Option<String>
// ═══════════════════════════════════════════════════════

#[handler]
async fn optional_path(#[path("id")] id: Option<String>) -> String {
    id.unwrap_or_else(|| "none".into())
}

#[tokio::test]
async fn handler_optional_path_present() {
    let router = Router::new().route("/items/:id", get(optional_path));
    let client = memory(router);
    let resp = client.get("/items/42").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "42");
}

#[tokio::test]
async fn handler_optional_path_missing() {
    // No :id in the pattern, so path_param returns None -> Option is None
    let router = Router::new().route("/items", get(optional_path));
    let client = memory(router);
    let resp = client.get("/items").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "none");
}
