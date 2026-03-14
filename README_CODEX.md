[![crates.io](https://img.shields.io/crates/v/crossbar.svg)](https://crates.io/crates/crossbar)
[![docs.rs](https://docs.rs/crossbar/badge.svg)](https://docs.rs/crossbar)
[![license](https://img.shields.io/crates/l/crossbar.svg)](https://github.com/anthropics/crossbar#license)
[![CI](https://github.com/anthropics/crossbar/actions/workflows/ci.yml/badge.svg)](https://github.com/anthropics/crossbar/actions/workflows/ci.yml)

# crossbar

Transport-polymorphic URI routing for Rust.

Define handlers once and serve them over in-process memory, Tokio channels, shared memory, Unix domain sockets, or TCP without changing your application code.

Crossbar is built for low-latency, high-throughput Rust systems such as trading infrastructure, game servers, inter-process bridges, robotics control planes, and market data distribution.

## Highlights

| Capability | What it gives you |
| --- | --- |
| One router, many transports | Reuse the same `Router`, handlers, and `Request`/`Response` types across memory, channel, shared-memory, UDS, and TCP backends |
| Low-latency request/response | Start in-process at sub-µs latency and move outward to IPC or network transports as deployment constraints change |
| URI routing | Match static paths and `:params`, parse query strings, and percent-decode path/query values |
| Ergonomic handlers | Use async handlers, sync handlers, closures, `Result`, `Json<T>`, `IntoResponse`, `#[handler]`, and `#[derive(IntoResponse)]` |
| Shared-memory RPC | Use mmap-backed request/response transport via `/dev/shm` with the `shm` feature on Unix |
| Zero-copy pub/sub | Publish raw bytes into shared memory and let subscribers read samples in-place with seqlock consistency and low wake-up overhead |
| Minimal wire format | UDS and TCP share a compact binary protocol instead of HTTP framing and header overhead |

## Why crossbar

Crossbar keeps routing and business logic separate from transport choice. The same handler can serve:

- another function in the same process
- another Tokio task
- another process on the same machine
- another service over a Unix socket
- another machine over TCP

That makes it useful when a system starts as an in-process component, then needs to move across task, process, or machine boundaries without a rewrite.

## Quick start

### `Cargo.toml`

```toml
[dependencies]
crossbar = "0.1.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
serde = { version = "1", features = ["derive"] }
```

### Minimal example

```rust
use crossbar::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

#[derive(Deserialize, Serialize)]
struct Echo {
    message: String,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn echo(req: Request) -> Result<Json<Echo>, (u16, &'static str)> {
    let payload: Echo = req.json_body().map_err(|_| (400, "invalid JSON body"))?;
    Ok(Json(payload))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new()
        .route("/health", get(health))
        .route("/echo", post(echo));

    let mem = InProcessClient::new(router.clone());
    let resp = mem.get("/health").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), r#"{"status":"ok"}"#);

    let chan = ChannelServer::spawn(router);
    let resp = chan
        .post("/echo", r#"{"message":"hello"}"#)
        .await?;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), r#"{"message":"hello"}"#);

    Ok(())
}
```

## Transport comparison

| Transport | Scope | Typical latency | Notes |
| --- | --- | --- | --- |
| `InProcessClient` | In-process | sub-µs | Direct router dispatch, no framing, no syscalls |
| `ChannelServer::spawn()` | Cross-task | 1-5 µs | Tokio `mpsc` + `oneshot`, good for intra-process RPC |
| `ShmServer` / `ShmClient` | Cross-process | 2-5 µs | mmap-backed RPC via `/dev/shm`, requires `shm` feature and Unix |
| `UdsServer` / `UdsClient` | Same host | 10-50 µs | Compact binary framing over Unix domain sockets |
| `TcpServer` / `TcpClient` | Network | 50-100 µs | Persistent TCP with `TCP_NODELAY` enabled |

Crossbar uses the same request model everywhere. Transport changes are deployment changes, not handler rewrites.

## Pub/Sub over shared memory

With the `shm` feature enabled, crossbar also exposes a zero-copy pub/sub path inspired by iceoryx-style shared-memory messaging.

### Properties

| Property | Behavior |
| --- | --- |
| Publish path | One memcpy into mmap plus two atomic stores |
| Read path | `try_recv_ref()` reads directly from shared memory with no allocation |
| Consistency | Seqlock-checked samples prevent torn reads |
| Notification | Futex-based wakeups on Linux, timed polling fallback on other Unix targets |
| Topology | Ring buffer per topic with configurable `ring_depth` |
| Topic lookup | FNV-1a hashing plus full URI comparison to guard against collisions |
| Async support | Cancellation-safe `recv_async()` |
| Failure detection | Heartbeat-based publisher liveness detection |

### Publisher example

```rust
#[cfg(all(unix, feature = "shm"))]
use crossbar::prelude::*;

#[cfg(all(unix, feature = "shm"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut publisher = ShmPublisher::create(
        "prices",
        PubSubConfig {
            sample_capacity: 1024,
            ring_depth: 64,
            max_topics: 16,
            ..PubSubConfig::default()
        },
    )?;

    let topic = publisher.register("/ticks/AAPL")?;

    let mut loan = publisher.loan_to(&topic);
    loan.set_data(br#"{"symbol":"AAPL","price":182.63}"#);
    loan.publish();

    Ok(())
}
```

### Subscriber example

```rust
#[cfg(all(unix, feature = "shm"))]
use crossbar::prelude::*;

#[cfg(all(unix, feature = "shm"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = ShmSubscriber::connect("prices")?;
    let mut stream = subscriber.subscribe("/ticks/AAPL")?;

    if let Some(sample) = stream.try_recv_ref() {
        let bytes: &[u8] = &sample;
        println!("{}", std::str::from_utf8(bytes)?);
    }

    Ok(())
}
```

Use `try_recv_ref()` for the absolute lowest overhead when consumers can process data immediately, or `recv()`, `recv_async()`, or `try_recv()` when you want owned, seqlock-checked samples and stronger consistency guarantees.

## Handler ergonomics

Crossbar deliberately mirrors the shape of modern Rust web frameworks while staying transport-agnostic.

### Async handlers

```rust
use crossbar::prelude::*;

async fn health() -> &'static str {
    "ok"
}

async fn echo(req: Request) -> Vec<u8> {
    req.body.to_vec()
}

let router = Router::new()
    .route("/health", get(health))
    .route("/echo", post(echo));
```

### Sync handlers

```rust
use crossbar::prelude::*;

fn health() -> &'static str {
    "ok"
}

fn body_len(req: Request) -> String {
    format!("{} bytes", req.body.len())
}

let router = Router::new()
    .route("/health", get(sync_handler(health)))
    .route("/len", post(sync_handler_with_req(body_len)));
```

### Extractors with `#[handler]`

```rust
use crossbar::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct UpdatePrice {
    price: f64,
}

#[handler]
async fn update_asset(
    #[path("symbol")] symbol: String,
    #[query("venue")] venue: Option<String>,
    #[body] payload: UpdatePrice,
) -> String {
    let venue = venue.unwrap_or_else(|| "default".to_string());
    format!("{symbol}@{venue}={}", payload.price)
}

let router = Router::new().route("/assets/:symbol", post(update_asset));
```

### JSON responses

```rust
use crossbar::prelude::*;
use serde::Serialize;

#[derive(Serialize)]
struct Tick {
    symbol: String,
    price: f64,
}

async fn get_tick(req: Request) -> Json<Tick> {
    Json(Tick {
        symbol: req.path_param("symbol").unwrap_or("UNKNOWN").to_string(),
        price: 182.63,
    })
}

let router = Router::new().route("/tick/:symbol", get(get_tick));
```

### Derive `IntoResponse`

```rust
use crossbar::prelude::*;
use serde::Serialize;

#[derive(Serialize, IntoResponse)]
struct Snapshot {
    symbol: String,
    bid: f64,
    ask: f64,
}

async fn snapshot() -> Snapshot {
    Snapshot {
        symbol: "AAPL".into(),
        bid: 182.62,
        ask: 182.63,
    }
}

let router = Router::new().route("/snapshot", get(snapshot));
```

## Feature flags

| Feature | Default | Enables |
| --- | --- | --- |
| `shm` | No | Shared-memory request/response transport and zero-copy pub/sub (`ShmServer`, `ShmClient`, `ShmPublisher`, `ShmSubscriber`) via `memmap2` and `libc` |

Platform notes:

- `shm` is intended for Unix targets and uses `/dev/shm` on Linux, falling back to `/tmp` on other Unix systems.
- Unix domain sockets are available on Unix only.
- Memory, channel, and TCP transports are available without extra features.

## Installation

### Core crate

```toml
[dependencies]
crossbar = "0.1.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### With shared-memory RPC and pub/sub

```toml
[dependencies]
crossbar = { version = "0.1.0", features = ["shm"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

If you return JSON or parse JSON request bodies, add `serde` and `serde_json` in your application as usual.

## Architecture

Crossbar is organized into four layers:

1. `Router`: immutable route table with method matching, path parameters, query parsing, and percent-decoding.
2. `Handler`: async and sync handler adapters plus `IntoResponse` conversion.
3. `Types`: shared `Request`, `Response`, and `Json<T>` abstractions used by every transport.
4. `Transport`: memory, channel, UDS, TCP, and optional shared-memory backends.

UDS and TCP share the same compact binary frame format:

- Request header: `1B method + 4B uri_len + 4B body_len + 4B headers_len`
- Response header: `2B status + 4B body_len + 4B headers_len`

The shared-memory RPC path uses mmap-backed request/response slots with atomic state transitions. The pub/sub path uses a ring buffer per topic with seqlock validation, notification counters, and heartbeat monitoring.

## Benchmarks

Crossbar ships Criterion benchmarks in [`benches/transport.rs`](https://github.com/anthropics/crossbar/blob/main/benches/transport.rs). The full write-up lives in [`BENCHMARKS.md`](https://github.com/anthropics/crossbar/blob/main/BENCHMARKS.md).

Run them locally with:

```sh
cargo bench --features shm
```

Sample results from the repository benchmarks on a Linux workstation:

| Benchmark | Memory | Channel | SHM | UDS | TCP |
| --- | --- | --- | --- | --- | --- |
| `/health` | 151 ns | 6.2 µs | - | 13.4 µs | 32.3 µs |
| JSON + path params | 1.17 µs | 8.4 µs | - | 16.2 µs | 34.1 µs |
| 64 KiB response | 1.22 µs | 8.0 µs | 6.2 µs | 27.2 µs | 43.9 µs |
| 1 MiB response | 18.5 µs | 24.7 µs | 26.3 µs | 215.8 µs | 229.1 µs |

These are microbenchmarks, not guarantees. Expect different absolute numbers across CPUs, kernels, NUMA layouts, and workload shapes. The important signal is relative transport behavior on your hardware.

## Use cases

Crossbar is a good fit for:

- low-latency trading and market data systems
- game servers and simulation backplanes
- robotics and control-plane IPC
- sidecar and bridge processes
- local service meshes where HTTP overhead is unnecessary

## Contributing

Issues and pull requests are welcome at [github.com/anthropics/crossbar](https://github.com/anthropics/crossbar).

Before opening a PR, run:

```sh
cargo test
```

For shared-memory code paths, also run:

```sh
cargo test --features shm
cargo bench --features shm
```

When changing transport behavior, include benchmark notes or rationale in the PR description so latency and throughput tradeoffs are explicit.

## License

Licensed under either of:

- MIT license ([`LICENSE-MIT`](https://github.com/anthropics/crossbar/blob/main/LICENSE-MIT))
- Apache License, Version 2.0 ([`LICENSE-APACHE`](https://github.com/anthropics/crossbar/blob/main/LICENSE-APACHE))

at your option.