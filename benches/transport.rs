use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use crossbar::prelude::*;
use std::hint::black_box;
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
// 1. Dispatch -- router dispatch only (via InProcessClient, zero transport)
// ====================================================

fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = InProcessClient::new(make_router());

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
// 2. In-process -- InProcessClient
// ====================================================

fn bench_inproc(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = InProcessClient::new(make_router());
    let body = order_json_body();

    let mut group = c.benchmark_group("inproc");
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

    // -- Transport overhead: publish (with futex wake) + recv --
    // Measures full publish path including futex_wake syscall.
    {
        let mut group = c.benchmark_group("pubsub_transport_only");
        group.measurement_time(Duration::from_secs(1));

        group.bench_function("with_wake", |b| {
            b.iter(|| {
                let loan = pub_.loan_to(&h_64b);
                loan.publish(); // includes futex_wake_all
                let s = s_64b.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        // Same but without futex wake — pure atomic overhead only.
        // This is the iceoryx-comparable number.
        group.bench_function("silent_no_wake", |b| {
            b.iter(|| {
                let loan = pub_.loan_to(&h_64b);
                loan.publish_silent(); // no futex, no notification counter
                let s = s_64b.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.finish();
    }

    // -- born-in-SHM: write directly into loaned buffer (iceoryx pattern) --
    // This is how iceoryx achieves O(1): data is constructed IN the SHM slot,
    // not copied from an external buffer. Only 8 bytes of metadata are written
    // regardless of topic capacity.
    {
        let mut group = c.benchmark_group("pubsub_born_in_shm");
        group.measurement_time(Duration::from_secs(1));

        group.bench_function("8B_on_64b_topic", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64b);
                let buf = loan.as_mut_slice();
                buf[0..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish();
                let s = s_64b.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.bench_function("8B_on_64kb_topic", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64kb);
                let buf = loan.as_mut_slice();
                buf[0..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish();
                let s = s_64kb.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.bench_function("8B_on_1mb_topic", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_1mb);
                let buf = loan.as_mut_slice();
                buf[0..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish();
                let s = s_1mb.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.finish();
    }

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
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.bench_function("64KB", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64kb);
                loan.as_mut_slice()[..payload_64kb.len()].copy_from_slice(&payload_64kb);
                loan.set_len(payload_64kb.len());
                loan.publish();
                let s = s_64kb.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.bench_function("1MB", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_1mb);
                loan.as_mut_slice()[..payload_1mb.len()].copy_from_slice(&payload_1mb);
                loan.set_len(payload_1mb.len());
                loan.publish();
                let s = s_1mb.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
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
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.bench_function("64kb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_64kb);
                loan.set_data(&payload_64kb);
                loan.publish();
                let s = s_64kb.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
            })
        });

        group.bench_function("1mb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan_to(&h_1mb);
                loan.set_data(&payload_1mb);
                loan.publish();
                let s = s_1mb.try_recv_ref().unwrap();
                black_box(unsafe { s.as_bytes_unchecked() });
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

    // In-process client
    let mem = InProcessClient::new(make_router());

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

        group.bench_function("inproc", |b| {
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

        group.bench_function("inproc", |b| {
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
// 5. Pool pub/sub -- O(1) zero-copy (shm feature)
// ====================================================

#[cfg(all(unix, feature = "shm"))]
fn bench_pool_pubsub(c: &mut Criterion) {
    let ps_name = "crossbar-bench-pool-ps";
    let cfg = PoolPubSubConfig {
        block_size: 1_048_576 + 8, // 1 MB data + 8B header
        block_count: 64,
        ring_depth: 8,
        ..PoolPubSubConfig::default()
    };
    let mut pub_ = ShmPoolPublisher::create(ps_name, cfg).unwrap();
    let h_8b = pub_.register("/bench/8b").unwrap();
    let h_64kb = pub_.register("/bench/64kb").unwrap();
    let h_1mb = pub_.register("/bench/1mb").unwrap();

    let sub = ShmPoolSubscriber::connect(ps_name).unwrap();
    let mut s_8b = sub.subscribe("/bench/8b").unwrap();
    let mut s_64kb = sub.subscribe("/bench/64kb").unwrap();
    let mut s_1mb = sub.subscribe("/bench/1mb").unwrap();

    let payload_64kb = vec![42u8; 65_536];
    let payload_1mb = vec![42u8; 1_048_576];

    // -- Minimal roundtrip: publish 8B + recv --
    // Measures end-to-end pub/sub with negligible payload write (8 bytes).
    {
        let mut group = c.benchmark_group("pool_pubsub_transport_only");
        group.measurement_time(Duration::from_secs(1));

        // Smart wake: publish() skips futex_wake when no subscriber is blocked
        // in recv(). Since benchmark uses try_recv(), waiters=0 — no futex syscall.
        // This is the apples-to-apples iceoryx2 comparison.
        group.bench_function("smart_wake", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_8b);
                loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish(); // smart: no futex since try_recv, not recv
                let g = s_8b.try_recv().unwrap();
                black_box(&*g);
            })
        });

        // Silent: no notification at all — pure atomics overhead floor.
        group.bench_function("silent_no_wake", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_8b);
                loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish_silent();
                let g = s_8b.try_recv().unwrap();
                black_box(&*g);
            })
        });

        group.finish();
    }

    // -- O(1) transfer: born-in-SHM + safe Deref read --
    {
        let mut group = c.benchmark_group("pool_pubsub_o1");
        group.measurement_time(Duration::from_secs(1));

        // 8 bytes — measures pure O(1) transfer overhead
        group.bench_function("8B", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_8b);
                loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
                loan.set_len(8);
                loan.publish();
                let g = s_8b.try_recv().unwrap();
                black_box(&*g); // safe Deref!
            })
        });

        // 64 KB — same O(1) ring transfer, different write cost
        group.bench_function("64KB", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_64kb);
                loan.as_mut_slice()[..payload_64kb.len()].copy_from_slice(&payload_64kb);
                loan.set_len(payload_64kb.len());
                loan.publish();
                let g = s_64kb.try_recv().unwrap();
                black_box(&*g);
            })
        });

        // 1 MB
        group.bench_function("1MB", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_1mb);
                loan.as_mut_slice()[..payload_1mb.len()].copy_from_slice(&payload_1mb);
                loan.set_len(payload_1mb.len());
                loan.publish();
                let g = s_1mb.try_recv().unwrap();
                black_box(&*g);
            })
        });

        group.finish();
    }

    // -- Throughput --
    {
        let mut group = c.benchmark_group("throughput_pool_pubsub");
        group.measurement_time(Duration::from_secs(3));

        group.throughput(Throughput::Bytes(65_536));
        group.bench_function("64kb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_64kb);
                loan.as_mut_slice()[..payload_64kb.len()].copy_from_slice(&payload_64kb);
                loan.set_len(payload_64kb.len());
                loan.publish();
                let g = s_64kb.try_recv().unwrap();
                black_box(&*g);
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("throughput_pool_pubsub_1mb");
        group.throughput(Throughput::Bytes(1_048_576));
        group.measurement_time(Duration::from_secs(3));

        group.bench_function("1mb", |b| {
            b.iter(|| {
                let mut loan = pub_.loan(&h_1mb);
                loan.as_mut_slice()[..payload_1mb.len()].copy_from_slice(&payload_1mb);
                loan.set_len(payload_1mb.len());
                loan.publish();
                let g = s_1mb.try_recv().unwrap();
                black_box(&*g);
            })
        });

        group.finish();
    }

    drop(pub_);
}

// ====================================================

criterion_group!(
    benches_common,
    bench_dispatch,
    bench_inproc,
    bench_throughput,
);

#[cfg(all(unix, feature = "shm"))]
criterion_group!(benches_shm, bench_shm,);

#[cfg(all(unix, feature = "shm"))]
criterion_group!(benches_pubsub, bench_pubsub,);

#[cfg(all(unix, feature = "shm"))]
criterion_group!(benches_pool_pubsub, bench_pool_pubsub,);

#[cfg(all(unix, feature = "shm"))]
criterion_main!(
    benches_common,
    benches_shm,
    benches_pubsub,
    benches_pool_pubsub
);

#[cfg(not(all(unix, feature = "shm")))]
criterion_main!(benches_common);
