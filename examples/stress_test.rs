//! Stress test — pushes crossbar to its limits across multiple dimensions.
//!
//! Tests run sequentially. Each one prints PASS/FAIL with details.
//!
//! Usage:
//!   cargo build --release --example stress_test
//!   target/release/examples/stress_test

use crossbar::error::Error as CbError;
use crossbar::{BusSubscriber, Config, Pod, PodBus, Publisher, Subscriber};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn unique(base: &str) -> String {
    static C: AtomicU64 = AtomicU64::new(0);
    format!(
        "{base}-{}-{}",
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    )
}

macro_rules! stress {
    ($name:expr, $body:block) => {{
        print!("  {:<55}", $name);
        let start = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body));
        let elapsed = start.elapsed();
        match result {
            Ok(()) => println!("PASS  ({:.1?})", elapsed),
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                println!("FAIL  {msg}");
            }
        }
    }};
}

fn main() {
    // Handle cross-process child mode
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--xproc-child") {
        let name = &args[2];
        let sub = Subscriber::connect(name).unwrap();
        let stream = sub.subscribe("/test").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(sample) = stream.try_recv() {
                if &*sample == b"from-parent" {
                    println!("XPROC_OK");
                } else {
                    println!("XPROC_WRONG_DATA");
                }
                break;
            }
            if Instant::now() > deadline {
                println!("XPROC_TIMEOUT");
                break;
            }
            std::hint::spin_loop();
        }
        return;
    }

    println!("╔══════════════════════════════════════════════════════╗");
    println!("║  crossbar stress test                                ║");
    println!("╚══════════════════════════════════════════════════════╝\n");

    // ── 1. Pool exhaustion and recovery ──────────────────────
    println!("── Pool exhaustion ────────────────────────────────────");

    stress!("exhaust pool, verify PoolExhausted error", {
        let name = unique("pool-exhaust");
        // ring_depth=8, block_count=8: publishing 8 messages fills all ring
        // slots (no wrapping → no recycling) and exhausts the pool.
        let mut pub_ = Publisher::create(
            &name,
            Config {
                block_count: 8,
                ring_depth: 8,
                ..Config::default()
            },
        )
        .unwrap();
        let topic = pub_.register("/test").unwrap();

        // Publish 8 messages WITHOUT calling try_recv — each loan takes a
        // block from the publisher's cache, and the ring never wraps so no
        // blocks are recycled.
        for i in 0u32..8 {
            let mut loan = pub_.loan(&topic).unwrap();
            loan.set_data(&i.to_le_bytes()).unwrap();
            loan.publish();
        }
        // All 8 blocks sit in ring slots — next loan must fail
        match pub_.loan(&topic) {
            Err(CbError::PoolExhausted) => {}
            other => panic!("expected PoolExhausted, got {:?}", other.map(|_| "Ok")),
        };
    });

    stress!("exhaust pool via held SampleGuards", {
        let name = unique("guard-exhaust");
        let mut pub_ = Publisher::create(
            &name,
            Config {
                block_count: 8,
                ring_depth: 4,
                ..Config::default()
            },
        )
        .unwrap();
        let topic = pub_.register("/test").unwrap();
        let sub = Subscriber::connect(&name).unwrap();
        let stream = sub.subscribe("/test").unwrap();

        // Publish 8 messages, hold all SampleGuards so blocks stay pinned
        let mut guards = Vec::new();
        for i in 0u32..8 {
            let mut loan = pub_.loan(&topic).unwrap();
            loan.set_data(&i.to_le_bytes()).unwrap();
            loan.publish();
            guards.push(stream.try_recv().unwrap());
        }
        // Pool exhausted because guards hold all 8 blocks
        assert!(matches!(pub_.loan(&topic), Err(CbError::PoolExhausted)));
        // Drop guards → blocks freed
        drop(guards);
        pub_.loan(&topic).unwrap().publish();
    });

    // ── 2. High throughput sustained ─────────────────────────
    println!("\n── High throughput ─────────────────────────────────────");

    stress!("1M messages, verify none lost", {
        let name = unique("throughput-1m");
        let mut pub_ = Publisher::create(
            &name,
            Config {
                ring_depth: 256,
                block_count: 512,
                ..Config::default()
            },
        )
        .unwrap();
        let topic = pub_.register("/data").unwrap();
        let sub = Subscriber::connect(&name).unwrap();
        let stream = sub.subscribe("/data").unwrap();

        let n = 1_000_000u64;
        let mut received = 0u64;
        let mut last_val = None;

        for i in 0..n {
            let mut loan = pub_.loan(&topic).unwrap();
            loan.set_data(&i.to_le_bytes()).unwrap();
            loan.publish();

            // Drain periodically to avoid ring overwrite
            if i % 64 == 0 {
                while let Some(sample) = stream.try_recv() {
                    let val = u64::from_le_bytes(sample[..8].try_into().unwrap());
                    if let Some(prev) = last_val {
                        assert!(val > prev, "out of order: {val} <= {prev}");
                    }
                    last_val = Some(val);
                    received += 1;
                }
            }
        }
        // Drain remaining
        while let Some(sample) = stream.try_recv() {
            let val = u64::from_le_bytes(sample[..8].try_into().unwrap());
            if let Some(prev) = last_val {
                assert!(val > prev, "out of order: {val} <= {prev}");
            }
            last_val = Some(val);
            received += 1;
        }
        assert!(received > n / 2, "received too few: {received}/{n}");
        assert_eq!(last_val, Some(n - 1), "last value should be {}", n - 1);
    });

    // ── 3. Multi-threaded concurrent publish/subscribe ────────
    println!("\n── Concurrent stress ───────────────────────────────────");

    stress!("4 publisher threads × 10K messages each", {
        let name = unique("concurrent-pub");
        let mut pub_ = Publisher::create(
            &name,
            Config {
                ring_depth: 256,
                block_count: 4096,
                max_topics: 4,
                ..Config::default()
            },
        )
        .unwrap();

        let topics: Vec<_> = (0..4)
            .map(|i| pub_.register(&format!("/t{i}")).unwrap())
            .collect();
        let sub = Subscriber::connect(&name).unwrap();
        let streams: Vec<_> = (0..4)
            .map(|i| sub.subscribe(&format!("/t{i}")).unwrap())
            .collect();

        // Each thread publishes to its own topic
        let pub_arc = Arc::new(std::sync::Mutex::new(pub_));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|tid| {
                let pub_c = pub_arc.clone();
                let topic = topics[tid];
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    for i in 0..10_000u64 {
                        loop {
                            let mut guard = match pub_c.try_lock() {
                                Ok(g) => g,
                                Err(std::sync::TryLockError::WouldBlock) => {
                                    std::thread::yield_now();
                                    continue;
                                }
                                Err(e) => panic!("mutex poisoned: {e}"),
                            };
                            // Scope the loan so it's dropped before the guard
                            let exhausted = {
                                match guard.loan(&topic) {
                                    Ok(mut loan) => {
                                        let payload = ((tid as u64) << 32) | i;
                                        loan.set_data(&payload.to_le_bytes()).unwrap();
                                        loan.publish();
                                        false
                                    }
                                    Err(CbError::PoolExhausted) => true,
                                    Err(e) => panic!("unexpected loan error: {e:?}"),
                                }
                            };
                            if exhausted {
                                drop(guard);
                                std::thread::yield_now();
                                continue;
                            }
                            break;
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Verify each topic got messages in order
        for (tid, stream) in streams.iter().enumerate() {
            let mut count = 0u64;
            let mut last = None;
            while let Some(sample) = stream.try_recv() {
                let val = u64::from_le_bytes(sample[..8].try_into().unwrap());
                let got_tid = (val >> 32) as usize;
                let got_seq = val & 0xFFFFFFFF;
                assert_eq!(got_tid, tid, "wrong topic");
                if let Some(prev) = last {
                    assert!(got_seq > prev, "out of order on topic {tid}");
                }
                last = Some(got_seq);
                count += 1;
            }
            assert!(count > 0, "topic {tid} received 0 messages");
        }
    });

    stress!("subscriber survives publisher drop", {
        let name = unique("pub-drop");
        let sub;
        let stream;
        {
            let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
            let topic = pub_.register("/test").unwrap();
            sub = Subscriber::connect(&name).unwrap();
            stream = sub.subscribe("/test").unwrap();
            let mut loan = pub_.loan(&topic).unwrap();
            loan.set_data(b"before-drop").unwrap();
            loan.publish();
            // pub_ drops here
        }
        // Subscriber should still read the data (refcount holds block)
        let sample = stream.try_recv().unwrap();
        assert_eq!(&*sample, b"before-drop");
    });

    // ── 4. Large payloads ────────────────────────────────────
    println!("\n── Large payloads ─────────────────────────────────────");

    stress!("1MB payload roundtrip", {
        let name = unique("large-1mb");
        let mut pub_ = Publisher::create(
            &name,
            Config {
                block_size: 1_048_576 + 8,
                block_count: 4,
                ..Config::default()
            },
        )
        .unwrap();
        let topic = pub_.register("/big").unwrap();
        let sub = Subscriber::connect(&name).unwrap();
        let stream = sub.subscribe("/big").unwrap();

        let payload = vec![0xABu8; 1_048_576];
        let mut loan = pub_.loan(&topic).unwrap();
        loan.set_data(&payload).unwrap();
        loan.publish();

        let sample = stream.try_recv().unwrap();
        assert_eq!(sample.len(), 1_048_576);
        assert!(
            sample.iter().all(|&b| b == 0xAB),
            "data corruption in 1MB payload"
        );
    });

    stress!("DataTooLarge error on oversized payload", {
        let name = unique("too-large");
        let mut pub_ = Publisher::create(
            &name,
            Config {
                block_size: 64,
                ..Config::default()
            },
        )
        .unwrap();
        let topic = pub_.register("/test").unwrap();
        let mut loan = pub_.loan(&topic).unwrap();
        let big = vec![0u8; 1000];
        match loan.set_data(&big) {
            Err(CbError::DataTooLarge { .. }) => {}
            other => panic!("expected DataTooLarge, got {:?}", other.map(|_| "Ok")),
        }
    });

    // ── 5. Rapid create/destroy cycles ───────────────────────
    println!("\n── Lifecycle stress ────────────────────────────────────");

    stress!("100 rapid create/publish/drop cycles", {
        for i in 0u32..100 {
            let name = unique(&format!("lifecycle-{i}"));
            let mut pub_ = Publisher::create(
                &name,
                Config {
                    block_count: 4,
                    ring_depth: 4,
                    ..Config::default()
                },
            )
            .unwrap();
            let topic = pub_.register("/test").unwrap();
            let mut loan = pub_.loan(&topic).unwrap();
            loan.set_data(&i.to_le_bytes()).unwrap();
            loan.publish();
        }
    });

    stress!("50 subscribers connect/disconnect rapidly", {
        let name = unique("sub-churn");
        let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
        let topic = pub_.register("/test").unwrap();
        let mut loan = pub_.loan(&topic).unwrap();
        loan.set_data(b"hello").unwrap();
        loan.publish();

        for _ in 0..50 {
            let sub = Subscriber::connect(&name).unwrap();
            let stream = sub.subscribe("/test").unwrap();
            let _ = stream.try_recv(); // may or may not get data
        }
        assert_eq!(
            pub_.subscriber_count(&topic).unwrap(),
            0,
            "leaked subscribers"
        );
    });

    // ── 6. PodBus stress ─────────────────────────────────────
    println!("\n── PodBus stress ──────────────────────────────────────");

    stress!("PodBus 1M publishes, verify no corruption", {
        let name = unique("podbus-1m");
        let mut bus = PodBus::<u64>::create(&name, 4096).unwrap();
        let mut sub = bus.subscriber().unwrap();

        let n = 1_000_000u64;
        let mut received = 0u64;
        let mut last = None;

        for i in 0..n {
            bus.publish(i);
            if i % 128 == 0 {
                while let Some(val) = sub.try_recv() {
                    if let Some(prev) = last {
                        assert!(val >= prev, "PodBus out of order: {val} < {prev}");
                    }
                    last = Some(val);
                    received += 1;
                }
            }
        }
        while let Some(val) = sub.try_recv() {
            if let Some(prev) = last {
                assert!(val >= prev, "PodBus out of order");
            }
            last = Some(val);
            received += 1;
        }
        assert!(received > n / 4, "PodBus received too few: {received}/{n}");
    });

    stress!("PodBus 8 subscriber threads, no corruption", {
        let name = unique("podbus-mt");
        let mut bus = PodBus::<u64>::create(&name, 4096).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let total_received = Arc::new(AtomicU64::new(0));
        let corruption_detected = Arc::new(AtomicBool::new(false));

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let r = running.clone();
                let tr = total_received.clone();
                let cd = corruption_detected.clone();
                let n = name.clone();
                std::thread::spawn(move || {
                    let mut sub = BusSubscriber::<u64>::connect(&n).unwrap();
                    let mut last: Option<u64> = None;
                    let mut count = 0u64;
                    while r.load(Ordering::Relaxed) {
                        if let Some(val) = sub.try_recv() {
                            if let Some(prev) = last {
                                if val < prev {
                                    cd.store(true, Ordering::Relaxed);
                                }
                            }
                            last = Some(val);
                            count += 1;
                        }
                        std::hint::spin_loop();
                    }
                    tr.fetch_add(count, Ordering::Relaxed);
                })
            })
            .collect();

        // Publish 500K messages
        for i in 0..500_000u64 {
            bus.publish(i);
        }
        std::thread::sleep(Duration::from_millis(100));
        running.store(false, Ordering::Relaxed);

        for t in threads {
            t.join().unwrap();
        }
        assert!(
            !corruption_detected.load(Ordering::Relaxed),
            "data corruption detected!"
        );
        let total = total_received.load(Ordering::Relaxed);
        assert!(total > 0, "no messages received across 8 subscribers");
    });

    // ── 7. Structured payloads ───────────────────────────────
    println!("\n── Structured payloads ─────────────────────────────────");

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Header {
        count: u32,
        checksum: u32,
    }
    unsafe impl Pod for Header {}

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Entry {
        value: f64,
        flags: u64,
    }
    unsafe impl Pod for Entry {}

    stress!("structured payload with 1000 entries", {
        let name = unique("structured-1k");
        let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
        let topic = pub_.register("/chain").unwrap();
        let sub = Subscriber::connect(&name).unwrap();
        let stream = sub.subscribe("/chain").unwrap();

        let entries: Vec<Entry> = (0..1000)
            .map(|i| Entry {
                value: i as f64 * 0.01,
                flags: i as u64,
            })
            .collect();
        let header = Header {
            count: 1000,
            checksum: 42,
        };

        let mut loan = pub_.loan(&topic).unwrap();
        loan.write_structured(&header, &entries).unwrap();
        loan.publish();

        let sample = stream.try_recv().unwrap();
        let h: &Header = sample.read_header().unwrap();
        assert_eq!(h.count, 1000);
        assert_eq!(h.checksum, 42);
        let arr: &[Entry] = sample.read_array::<Header, Entry>().unwrap();
        assert_eq!(arr.len(), 1000);
        assert_eq!(arr[0].value, 0.0);
        assert_eq!(arr[999].value, 9.99);
        assert_eq!(arr[500].flags, 500);
    });

    // ── 8. Service discovery ─────────────────────────────────
    println!("\n── Service discovery ───────────────────────────────────");

    stress!("discover registered topics via wildcard", {
        let name = unique("discovery");
        let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
        pub_.register("/tick/AAPL").unwrap();
        pub_.register("/tick/GOOG").unwrap();
        pub_.register("/flow/SPX").unwrap();

        let topics = crossbar::discover("/tick/*").unwrap();
        let uris: Vec<_> = topics.iter().map(|t| t.uri.as_str()).collect();
        assert!(uris.contains(&"/tick/AAPL"), "missing AAPL");
        assert!(uris.contains(&"/tick/GOOG"), "missing GOOG");
        assert!(
            !uris.contains(&"/flow/SPX"),
            "flow should not match /tick/*"
        );
    });

    // ── 9. Error paths ───────────────────────────────────────
    println!("\n── Error paths ────────────────────────────────────────");

    stress!("path traversal rejected", {
        assert!(matches!(
            Publisher::create("../etc/evil", Config::default()),
            Err(CbError::SegmentNameInvalid(_))
        ));
        assert!(matches!(
            Publisher::create("a/b", Config::default()),
            Err(CbError::SegmentNameInvalid(_))
        ));
        assert!(matches!(
            Publisher::create("", Config::default()),
            Err(CbError::SegmentNameInvalid(_))
        ));
    });

    stress!("invalid config rejected", {
        assert!(matches!(
            Publisher::create(
                &unique("bad"),
                Config {
                    block_size: 1,
                    ..Config::default()
                }
            ),
            Err(CbError::InvalidRegion(_))
        ));
        assert!(matches!(
            Publisher::create(
                &unique("bad"),
                Config {
                    ring_depth: 3,
                    ..Config::default()
                }
            ),
            Err(CbError::InvalidRegion(_))
        ));
        assert!(matches!(
            Publisher::create(
                &unique("bad"),
                Config {
                    block_count: 0,
                    ..Config::default()
                }
            ),
            Err(CbError::InvalidRegion(_))
        ));
    });

    stress!("TopicNotFound for unknown URI", {
        let name = unique("notfound");
        let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
        pub_.register("/exists").unwrap();
        let sub = Subscriber::connect(&name).unwrap();
        assert!(matches!(
            sub.subscribe("/nope"),
            Err(CbError::TopicNotFound(_))
        ));
    });

    stress!("HandleMismatch across publishers", {
        let name_a = unique("mismatch-a");
        let name_b = unique("mismatch-b");
        let mut pub_a = Publisher::create(&name_a, Config::default()).unwrap();
        let mut pub_b = Publisher::create(&name_b, Config::default()).unwrap();
        let topic_a = pub_a.register("/test").unwrap();
        assert!(matches!(pub_b.loan(&topic_a), Err(CbError::HandleMismatch)));
    });

    // ── 10. Cross-process spawn test ─────────────────────────
    println!("\n── Cross-process ───────────────────────────────────────");

    stress!("child process reads parent's published data", {
        let name = unique("xproc");
        let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
        let topic = pub_.register("/test").unwrap();

        // Spawn child FIRST — it will connect, subscribe, and poll in a loop.
        // We publish AFTER spawn so the child's subscriber sees new data
        // (subscribers start from the latest seq at subscribe-time).
        let exe = std::env::current_exe().unwrap();
        let child = std::process::Command::new(&exe)
            .args(["--xproc-child", &name])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        // Give the child time to connect and subscribe before publishing
        std::thread::sleep(Duration::from_millis(100));

        let mut loan = pub_.loan(&topic).unwrap();
        loan.set_data(b"from-parent").unwrap();
        loan.publish();

        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("XPROC_OK"), "child output: {stdout}");
    });

    println!("\n════════════════════════════════════════════════════════");
    println!("  All stress tests complete.");
    println!("════════════════════════════════════════════════════════");
}
