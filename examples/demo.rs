use crossbar::prelude::*;
use serde::{Deserialize, Serialize};

// -- Handlers ----
// Same handlers serve over ALL transports -- memory and SHM.

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct OhlcData {
    symbol: String,
    venue: String,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: u64,
}

async fn get_ohlc(req: Request) -> Json<OhlcData> {
    let symbol = req.path_param("symbol").unwrap_or("???").to_string();
    let venue = req.query_param("venue").unwrap_or_else(|| "default".into());
    Json(OhlcData {
        symbol,
        venue,
        open: 150.25,
        high: 155.80,
        low: 149.10,
        close: 153.42,
        volume: 48_392_100,
    })
}

#[derive(Deserialize)]
struct OrderRequest {
    symbol: String,
    side: String,
    qty: u32,
}

#[derive(Serialize)]
struct OrderResponse {
    order_id: String,
    symbol: String,
    side: String,
    qty: u32,
    status: String,
}

async fn create_order(req: Request) -> Result<Json<OrderResponse>, (u16, &'static str)> {
    let order: OrderRequest = req.json_body().map_err(|_| (400u16, "invalid JSON body"))?;
    Ok(Json(OrderResponse {
        order_id: "ORD-000042".into(),
        symbol: order.symbol,
        side: order.side,
        qty: order.qty,
        status: "filled".into(),
    }))
}

// -- Main ----

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!();
    println!("  +==========================================================+");
    println!("  |  CROSSBAR -- Transport-Polymorphic URI Router             |");
    println!("  |  Same handlers. Same URIs. Any transport.                |");
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

    // -- 1. Memory ---
    println!("\n  --- Memory (in-process, sub-us) ---");
    let mem = MemoryClient::new(router.clone());

    let r = mem.get("/health").await;
    println!("    GET  /health -> {} {}", r.status, r.body_str());

    let r = mem.get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb").await;
    println!(
        "    GET  /v3/stock/snapshot/ohlc/AAPL -> {} {}",
        r.status,
        truncate(r.body_str(), 60)
    );

    let r = mem.post("/v3/stock/order", order_body.clone()).await;
    println!(
        "    POST /v3/stock/order -> {} {}",
        r.status,
        truncate(r.body_str(), 60)
    );

    let r = mem.get("/nonexistent").await;
    println!("    GET  /nonexistent -> {}", r.status);

    // -- 2. SHM (shared memory, Linux/Unix) ---
    #[cfg(all(unix, feature = "shm"))]
    let shm = {
        let shm_name = "demo";
        println!("\n  --- Shared Memory (/dev/shm) ---");
        {
            let r = router.clone();
            tokio::spawn(async move {
                ShmServer::bind(shm_name, r).await.unwrap();
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let shm = ShmClient::connect(shm_name).await?;
        let r = shm.get("/health").await?;
        println!("    GET  /health -> {} {}", r.status, r.body_str());

        let r = shm.get("/v3/stock/snapshot/ohlc/NVDA?venue=nqb").await?;
        println!(
            "    GET  /v3/stock/snapshot/ohlc/NVDA -> {} {}",
            r.status,
            truncate(r.body_str(), 60)
        );

        let r = shm.post("/v3/stock/order", order_body.clone()).await?;
        println!(
            "    POST /v3/stock/order -> {} {}",
            r.status,
            truncate(r.body_str(), 60)
        );

        shm
    };

    // -- Latency comparison ---
    println!("\n  --- Latency Comparison ---");
    println!("    Warming up...");
    let uri = "/v3/stock/snapshot/ohlc/AAPL?venue=nqb";
    let n_warmup = 500;
    let n_measure = 5000;

    // Warm up all transports
    for _ in 0..n_warmup {
        mem.get(uri).await;
        #[cfg(all(unix, feature = "shm"))]
        shm.get(uri).await.unwrap();
    }

    // Measure memory
    let stats_mem = bench_transport(n_measure, || mem.get(uri)).await;

    // Measure SHM
    #[cfg(all(unix, feature = "shm"))]
    let stats_shm = bench_transport(n_measure, || async { shm.get(uri).await.unwrap() }).await;

    println!();
    println!(
        "    {:<10} {:>10} {:>10} {:>10} {:>10}",
        "Transport", "min", "avg", "p99", "max"
    );
    println!("    {}", "-".repeat(54));
    print_stats("Memory", &stats_mem);
    #[cfg(all(unix, feature = "shm"))]
    print_stats("SHM", &stats_shm);

    println!();
    println!("    One router. Two transports. Same URIs. Same handlers.");
    println!();

    // Cleanup
    #[cfg(all(unix, feature = "shm"))]
    let _ = std::fs::remove_file("/dev/shm/crossbar-demo");

    Ok(())
}

// -- Bench helpers ----

struct Stats {
    min_ns: u128,
    avg_ns: u128,
    p99_ns: u128,
    max_ns: u128,
}

async fn bench_transport<F, Fut>(n: usize, f: F) -> Stats
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let start = std::time::Instant::now();
        let _ = std::hint::black_box(f().await);
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
