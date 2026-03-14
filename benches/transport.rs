use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use crossbar::prelude::*;
#[cfg(all(unix, feature = "shm"))]
use std::sync::Arc;
use std::time::Duration;

// -- Handlers ----

async fn health() -> &'static str {
    "ok"
}

async fn get_ohlc(req: Request) -> Json<serde_json::Value> {
    let symbol = req.path_param("symbol").unwrap_or("???");
    Json(serde_json::json!({
        "symbol": symbol,
        "open": 150.25,
        "high": 155.80,
        "low": 149.10,
        "close": 153.42
    }))
}

async fn post_order(req: Request) -> Json<serde_json::Value> {
    let body: serde_json::Value = req.json_body().unwrap_or_default();
    Json(serde_json::json!({"status": "filled", "order": body}))
}

async fn large_payload_64k(_req: Request) -> Response {
    Response::ok().with_body(vec![42u8; 65_536])
}

async fn large_payload_1m(_req: Request) -> Response {
    Response::ok().with_body(vec![42u8; 1_048_576])
}

// -- Shared router ----

fn make_router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v3/stock/snapshot/ohlc/:symbol", get(get_ohlc))
        .route("/v3/stock/order", post(post_order))
        .route("/large/64k", get(large_payload_64k))
        .route("/large/1m", get(large_payload_1m))
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
// 1. Dispatch -- router dispatch only (via MemoryClient, zero transport)
// ====================================================

fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = MemoryClient::new(make_router());

    let mut group = c.benchmark_group("dispatch");
    group.measurement_time(Duration::from_millis(500));

    group.bench_function("health", |b| {
        let client = &client;
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/health").await) })
    });

    group.bench_function("ohlc_with_params", |b| {
        let client = &client;
        b.to_async(&rt).iter(|| async {
            black_box(client.get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb").await)
        })
    });

    group.bench_function("404", |b| {
        let client = &client;
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/nonexistent/route").await) })
    });

    group.finish();
}

// ====================================================
// 2. Memory -- in-process MemoryClient
// ====================================================

fn bench_memory(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = MemoryClient::new(make_router());
    let body = order_json_body();

    let mut group = c.benchmark_group("memory");
    group.measurement_time(Duration::from_millis(500));

    group.bench_function("health", |b| {
        let client = &client;
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/health").await) })
    });

    group.bench_function("ohlc", |b| {
        let client = &client;
        b.to_async(&rt).iter(|| async {
            black_box(client.get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb").await)
        })
    });

    group.bench_function("post_json", |b| {
        let client = &client;
        let body = body.clone();
        b.to_async(&rt).iter(|| {
            let body = body.clone();
            async { black_box(client.post("/v3/stock/order", body).await) }
        })
    });

    group.bench_function("large_64kb", |b| {
        let client = &client;
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/large/64k").await) })
    });

    group.bench_function("large_1mb", |b| {
        let client = &client;
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/large/1m").await) })
    });

    group.finish();
}

// ====================================================
// 3. SHM -- Shared Memory (shm feature)
// ====================================================

#[cfg(all(unix, feature = "shm"))]
fn bench_shm(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shm_name = "crossbar-bench";
    let body = order_json_body();

    let _handle = rt.block_on(async { ShmServer::spawn(shm_name, make_router()).await.unwrap() });

    let client = Arc::new(rt.block_on(ShmClient::connect(shm_name)).unwrap());

    let mut group = c.benchmark_group("shm");
    group.measurement_time(Duration::from_secs(1));

    group.bench_function("health", |b| {
        let client = client.clone();
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/health").await.unwrap()) })
    });

    group.bench_function("ohlc", |b| {
        let client = client.clone();
        b.to_async(&rt).iter(|| async {
            black_box(
                client
                    .get("/v3/stock/snapshot/ohlc/AAPL?venue=nqb")
                    .await
                    .unwrap(),
            )
        })
    });

    group.bench_function("post_json", |b| {
        let client = client.clone();
        let body = body.clone();
        b.to_async(&rt).iter(|| {
            let body = body.clone();
            async { black_box(client.post("/v3/stock/order", body).await.unwrap()) }
        })
    });

    group.bench_function("large_64kb", |b| {
        let client = client.clone();
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/large/64k").await.unwrap()) })
    });

    group.bench_function("large_1mb", |b| {
        let client = client.clone();
        b.to_async(&rt)
            .iter(|| async { black_box(client.get("/large/1m").await.unwrap()) })
    });

    group.finish();

    let _ = std::fs::remove_file("/dev/shm/crossbar-crossbar-bench");
}

// ====================================================
// 3b. Pub/Sub -- zero-copy SHM (shm feature)
// ====================================================

#[cfg(all(unix, feature = "shm"))]
fn bench_pubsub(c: &mut Criterion) {
    // Create publisher with enough capacity for 1 MB samples
    let ps_name = "crossbar-bench-ps";
    let cfg = PubSubConfig {
        sample_capacity: 1_048_576,
        max_topics: 3, // only 3 topics registered
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(ps_name, cfg).unwrap();
    let h_64b = pub_.register("/bench/64b").unwrap();
    let h_64kb = pub_.register("/bench/64kb").unwrap();
    let h_1mb = pub_.register("/bench/1mb").unwrap();

    let sub = ShmSubscriber::connect(ps_name).unwrap();
    let mut s_64b = sub.subscribe("/bench/64b").unwrap();
    let mut s_64kb = sub.subscribe("/bench/64kb").unwrap();
    let mut s_1mb = sub.subscribe("/bench/1mb").unwrap();

    let payload_64b = vec![42u8; 64];
    let payload_64kb = vec![42u8; 65_536];
    let payload_1mb = vec![42u8; 1_048_576];

    // -- Zero-copy path: memcpy write + zero-copy read --
    {
        let mut group = c.benchmark_group("pubsub_zerocopy");
        group.measurement_time(Duration::from_secs(1));

        group.bench_function("8B", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64b);
                let buf = loan.as_mut_slice();
                buf[0..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish();
                let s = s_64b.try_recv_ref().unwrap();
                black_box(&*s);
            })
        });

        group.bench_function("64KB", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64kb);
                loan.as_mut_slice()[..payload_64kb.len()].copy_from_slice(&payload_64kb);
                loan.set_len(payload_64kb.len());
                loan.publish();
                let s = s_64kb.try_recv_ref().unwrap();
                black_box(&*s);
            })
        });

        group.bench_function("1MB", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_1mb);
                loan.as_mut_slice()[..payload_1mb.len()].copy_from_slice(&payload_1mb);
                loan.set_len(payload_1mb.len());
                loan.publish();
                let s = s_1mb.try_recv_ref().unwrap();
                black_box(&*s);
            })
        });

        group.finish();
    }

    // -- memcpy path: write_all full payload --
    {
        let mut group = c.benchmark_group("pubsub_memcpy");
        group.measurement_time(Duration::from_secs(1));

        group.bench_function("64_bytes", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64b);
                loan.set_data(&payload_64b);
                loan.publish();
                let s = s_64b.try_recv_ref().unwrap();
                black_box(&*s);
            })
        });

        group.bench_function("64kb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64kb);
                loan.set_data(&payload_64kb);
                loan.publish();
                let s = s_64kb.try_recv_ref().unwrap();
                black_box(&*s);
            })
        });

        group.bench_function("1mb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_1mb);
                loan.set_data(&payload_1mb);
                loan.publish();
                let s = s_1mb.try_recv_ref().unwrap();
                black_box(&*s);
            })
        });

        group.finish();
    }

    // Throughput measurements
    {
        let mut group = c.benchmark_group("throughput_pubsub");
        group.measurement_time(Duration::from_secs(3));

        group.throughput(Throughput::Bytes(65_536));
        group.bench_function("64kb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64kb);
                loan.set_data(&payload_64kb);
                loan.publish();
                black_box(s_64kb.try_recv().unwrap());
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("throughput_pubsub_1mb");
        group.throughput(Throughput::Bytes(1_048_576));
        group.measurement_time(Duration::from_secs(3));

        group.bench_function("1mb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_1mb);
                loan.set_data(&payload_1mb);
                loan.publish();
                black_box(s_1mb.try_recv().unwrap());
            })
        });

        group.finish();
    }

    drop(pub_); // removes /dev/shm file
}

// ====================================================
// 4. Throughput -- bytes/sec measurements for large payloads
// ====================================================

fn bench_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Memory client
    let mem = MemoryClient::new(make_router());

    // SHM client (separate name from the shm group)
    #[cfg(all(unix, feature = "shm"))]
    let (_shm_handle, shm) = {
        let shm_name = "crossbar-bench-tp";
        let handle =
            rt.block_on(async { ShmServer::spawn(shm_name, make_router()).await.unwrap() });
        let client = Arc::new(rt.block_on(ShmClient::connect(shm_name)).unwrap());
        (handle, client)
    };

    // -- 64KB throughput --
    {
        let mut group = c.benchmark_group("throughput/64kb");
        group.throughput(Throughput::Bytes(65_536));
        group.measurement_time(Duration::from_secs(3));

        group.bench_function("memory", |b| {
            let mem = &mem;
            b.to_async(&rt)
                .iter(|| async { black_box(mem.get("/large/64k").await) })
        });

        #[cfg(all(unix, feature = "shm"))]
        group.bench_function("shm", |b| {
            let shm = shm.clone();
            b.to_async(&rt)
                .iter(|| async { black_box(shm.get("/large/64k").await.unwrap()) })
        });

        group.finish();
    }

    // -- 1MB throughput --
    {
        let mut group = c.benchmark_group("throughput/1mb");
        group.throughput(Throughput::Bytes(1_048_576));
        group.measurement_time(Duration::from_secs(3));

        group.bench_function("memory", |b| {
            let mem = &mem;
            b.to_async(&rt)
                .iter(|| async { black_box(mem.get("/large/1m").await) })
        });

        #[cfg(all(unix, feature = "shm"))]
        group.bench_function("shm", |b| {
            let shm = shm.clone();
            b.to_async(&rt)
                .iter(|| async { black_box(shm.get("/large/1m").await.unwrap()) })
        });

        group.finish();
    }

    // Cleanup
    #[cfg(all(unix, feature = "shm"))]
    let _ = std::fs::remove_file("/dev/shm/crossbar-crossbar-bench-tp");
}

// ====================================================

criterion_group!(
    benches_common,
    bench_dispatch,
    bench_memory,
    bench_throughput,
);

#[cfg(all(unix, feature = "shm"))]
criterion_group!(benches_shm, bench_shm,);

#[cfg(all(unix, feature = "shm"))]
criterion_group!(benches_pubsub, bench_pubsub,);

#[cfg(all(unix, feature = "shm"))]
criterion_main!(benches_common, benches_shm, benches_pubsub);

#[cfg(not(all(unix, feature = "shm")))]
criterion_main!(benches_common);
