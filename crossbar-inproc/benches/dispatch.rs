use criterion::{criterion_group, criterion_main, Criterion};
use crossbar_inproc::prelude::*;
use std::hint::black_box;
use std::time::Duration;

// -- Handlers ----

fn health() -> &'static str {
    "ok"
}

fn get_ohlc(req: Request) -> Response {
    let symbol = req.path_param("symbol").unwrap_or("???");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "symbol": symbol,
        "open": 150.25,
        "high": 155.80,
        "low": 149.10,
        "close": 153.42
    }))
    .unwrap();
    Response::ok()
        .with_body(bytes)
        .with_header("content-type", "application/json")
}

fn post_order(req: Request) -> Response {
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let bytes =
        serde_json::to_vec(&serde_json::json!({"status": "filled", "order": body})).unwrap();
    Response::ok()
        .with_body(bytes)
        .with_header("content-type", "application/json")
}

fn large_payload_64k(_req: Request) -> Response {
    Response::ok().with_body(vec![42u8; 65_536])
}

fn large_payload_1m(_req: Request) -> Response {
    Response::ok().with_body(vec![42u8; 1_048_576])
}

// -- Shared router ----

fn make_router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/large/64k", get(large_payload_64k))
        .route("/large/1m", get(large_payload_1m))
        .route("/v3/stock/snapshot/ohlc/:symbol", get(get_ohlc))
        .route("/v3/stock/order", post(post_order))
}

// -- JSON body for POST benchmarks ----

fn order_json_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "symbol": "AAPL",
        "side": "buy",
        "qty": 100,
        "price": 152.50
    }))
    .unwrap()
}

// ====================================================
// 1. Dispatch -- router dispatch only (via InProcessClient, zero transport)
// ====================================================

fn bench_dispatch(c: &mut Criterion) {
    let client = InProcessClient::new(make_router());

    let mut group = c.benchmark_group("dispatch");
    group.measurement_time(Duration::from_millis(500));

    group.bench_function("health", |b| {
        let client = &client;
        b.iter(|| black_box(client.get("/health")))
    });

    group.bench_function("ohlc_with_params", |b| {
        let client = &client;
        b.iter(|| black_box(client.get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb")))
    });

    group.bench_function("404", |b| {
        let client = &client;
        b.iter(|| black_box(client.get("/nonexistent/route")))
    });

    group.finish();
}

// ====================================================
// 2. In-process -- InProcessClient
// ====================================================

fn bench_inproc(c: &mut Criterion) {
    let client = InProcessClient::new(make_router());

    let mut group = c.benchmark_group("inproc");
    group.measurement_time(Duration::from_millis(500));

    group.bench_function("health", |b| {
        let client = &client;
        b.iter(|| black_box(client.get("/health")))
    });

    {
        let body = order_json_body();

        group.bench_function("ohlc", |b| {
            let client = &client;
            b.iter(|| black_box(client.get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb")))
        });

        group.bench_function("post_json", |b| {
            let client = &client;
            let body = body.clone();
            b.iter(|| {
                let body = body.clone();
                black_box(client.post("/v3/stock/order", body))
            })
        });
    }

    group.bench_function("large_64kb", |b| {
        let client = &client;
        b.iter(|| black_box(client.get("/large/64k")))
    });

    group.bench_function("large_1mb", |b| {
        let client = &client;
        b.iter(|| black_box(client.get("/large/1m")))
    });

    group.finish();
}

// ====================================================

criterion_group!(benches, bench_dispatch, bench_inproc);
criterion_main!(benches);
