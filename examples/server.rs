use crossbar::prelude::*;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

// ── Handlers ────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct Tick {
    symbol: String,
    price: f64,
    volume: u64,
    ts: u64,
}

async fn get_tick(req: Request) -> Json<Tick> {
    let symbol = req.path_param("symbol").unwrap_or("UNKNOWN").to_uppercase();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    Json(Tick {
        symbol,
        price: 182.63,
        volume: 48_392_100,
        ts,
    })
}

async fn echo(req: Request) -> Vec<u8> {
    req.body.to_vec()
}

// ── Main ────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new()
        .route("/health", get(health))
        .route("/tick/:symbol", get(get_tick))
        .route("/echo", post(echo));

    let addr = "127.0.0.1:4000";
    println!("crossbar server listening on {addr}");
    println!();
    println!("  GET  /health");
    println!("  GET  /tick/:symbol");
    println!("  POST /echo");
    println!();
    println!("Try:  cargo run --example client");

    TcpServer::bind(addr, router).await?;

    Ok(())
}
