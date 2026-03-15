use crossbar_inproc::prelude::*;

// -- Handlers ----
// In-process endpoint-style dispatch.

fn health() -> &'static str {
    "ok"
}

#[derive(serde::Serialize)]
struct OhlcData {
    symbol: String,
    venue: String,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: u64,
}

fn get_ohlc(req: Request) -> Response {
    let symbol = req.path_param("symbol").unwrap_or("???").to_string();
    let venue = req.query_param("venue").unwrap_or_else(|| "default".into());
    let data = OhlcData {
        symbol,
        venue,
        open: 150.25,
        high: 155.80,
        low: 149.10,
        close: 153.42,
        volume: 48_392_100,
    };
    let bytes = serde_json::to_vec(&data).unwrap();
    Response::ok()
        .with_body(bytes)
        .with_header("content-type", "application/json")
}

#[derive(serde::Deserialize)]
struct OrderRequest {
    symbol: String,
    side: String,
    qty: u32,
}

#[derive(serde::Serialize)]
struct OrderResponse {
    order_id: String,
    symbol: String,
    side: String,
    qty: u32,
    status: String,
}

fn create_order(req: Request) -> Response {
    let order: OrderRequest = match serde_json::from_slice(&req.body) {
        Ok(o) => o,
        Err(_) => return Response::bad_request("invalid JSON body"),
    };
    let resp = OrderResponse {
        order_id: "ORD-000042".into(),
        symbol: order.symbol,
        side: order.side,
        qty: order.qty,
        status: "filled".into(),
    };
    let bytes = serde_json::to_vec(&resp).unwrap();
    Response::ok()
        .with_body(bytes)
        .with_header("content-type", "application/json")
}

// -- Main ----

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!();
    println!("  +==========================================================+");
    println!("  |  CROSSBAR -- URI Router + Shared-Memory Streams          |");
    println!("  |  In-process dispatch with the same endpoint style.       |");
    println!("  +==========================================================+");
    println!();

    // Build router once
    let router = Router::new()
        .route("/health", get(health))
        .route("/v3/stock/snapshot/ohlc/:symbol", get(get_ohlc))
        .route("/v3/stock/order", post(create_order));

    // Show registered routes
    println!("  Routes:");
    for (method, pattern) in router.routes_info() {
        println!("    {:<6} {}", method, pattern);
    }

    let order_body = serde_json::to_vec(&serde_json::json!({
        "symbol": "AAPL", "side": "buy", "qty": 100
    }))?;

    // -- In-process ---
    println!("\n  --- In-process (sub-us) ---");
    let mem = InProcessClient::new(router.clone());

    let r = mem.get("/health");
    println!("    GET  /health -> {} {}", r.status, r.body_str());

    {
        let r = mem.get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb");
        println!(
            "    GET  /v3/stock/snapshot/ohlc/AAPL -> {} {}",
            r.status,
            truncate(r.body_str(), 60)
        );

        let r = mem.post("/v3/stock/order", order_body.clone());
        println!(
            "    POST /v3/stock/order -> {} {}",
            r.status,
            truncate(r.body_str(), 60)
        );
    }

    let r = mem.get("/nonexistent");
    println!("    GET  /nonexistent -> {}", r.status);

    // -- Latency comparison ---
    println!("\n  --- Latency Comparison ---");
    println!("    Warming up...");
    let uri = "/v3/stock/snapshot/ohlc/AAPL?venue=nqb";
    let n_warmup = 500;
    let n_measure = 5000;

    // Warm up
    for _ in 0..n_warmup {
        mem.get(uri);
    }

    // Measure in-process
    let stats_mem = bench_transport(n_measure, || mem.get(uri));

    println!();
    println!(
        "    {:<10} {:>10} {:>10} {:>10} {:>10}",
        "Transport", "min", "avg", "p99", "max"
    );
    println!("    {}", "-".repeat(54));
    print_stats("InProc", &stats_mem);

    println!();
    println!("    One router. Endpoint-style dispatch without HTTP.");
    println!();

    Ok(())
}

// -- Bench helpers ----

struct Stats {
    min_ns: u128,
    avg_ns: u128,
    p99_ns: u128,
    max_ns: u128,
}

fn bench_transport<F>(n: usize, f: F) -> Stats
where
    F: Fn() -> Response,
{
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let start = std::time::Instant::now();
        let _ = std::hint::black_box(f());
        times.push(start.elapsed().as_nanos());
    }
    times.sort_unstable();
    let sum: u128 = times.iter().sum();
    let p99_idx = (n as f64 * 0.99) as usize;
    Stats {
        min_ns: times[0],
        avg_ns: sum / n as u128,
        p99_ns: times[p99_idx.min(n - 1)],
        max_ns: *times.last().unwrap(),
    }
}

fn format_duration(ns: u128) -> String {
    if ns < 1_000 {
        format!("{} ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.1} us", ns as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    }
}

fn print_stats(name: &str, s: &Stats) {
    println!(
        "    {:<10} {:>10} {:>10} {:>10} {:>10}",
        name,
        format_duration(s.min_ns),
        format_duration(s.avg_ns),
        format_duration(s.p99_ns),
        format_duration(s.max_ns),
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
