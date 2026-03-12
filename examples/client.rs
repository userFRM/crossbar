use crossbar::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = TcpClient::connect("127.0.0.1:4000").await?;

    // Health check
    let resp = client.get("/health").await?;
    println!(
        "GET /health          -> {} {}",
        resp.status,
        resp.body_str()
    );

    // Fetch a market tick
    let resp = client.get("/tick/AAPL").await?;
    println!(
        "GET /tick/AAPL       -> {} {}",
        resp.status,
        resp.body_str()
    );

    // Echo binary data
    let resp = client.post("/echo", b"hello crossbar".to_vec()).await?;
    println!(
        "POST /echo           -> {} {}",
        resp.status,
        resp.body_str()
    );

    // 404
    let resp = client.get("/nonexistent").await?;
    println!("GET /nonexistent     -> {}", resp.status);

    Ok(())
}
