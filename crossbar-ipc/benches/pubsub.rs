use criterion::Throughput;
use criterion::{criterion_group, criterion_main, Criterion};
use crossbar_ipc::*;
use std::hint::black_box;
use std::time::Duration;

// ====================================================
// Pub/sub -- O(1) zero-copy
// ====================================================

fn bench_pubsub(c: &mut Criterion) {
    let ps_name = "crossbar-bench-ps";
    let cfg = PubSubConfig {
        block_size: 1_048_576 + 8, // 1 MB data + 8B header
        block_count: 64,
        ring_depth: 8,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(ps_name, cfg).unwrap();
    let h_8b = pub_.register("/bench/8b").unwrap();
    let h_64kb = pub_.register("/bench/64kb").unwrap();
    let h_1mb = pub_.register("/bench/1mb").unwrap();

    let sub = ShmSubscriber::connect(ps_name).unwrap();
    let s_8b = sub.subscribe("/bench/8b").unwrap();
    let s_64kb = sub.subscribe("/bench/64kb").unwrap();
    let s_1mb = sub.subscribe("/bench/1mb").unwrap();

    let payload_64kb = vec![42u8; 65_536];
    let payload_1mb = vec![42u8; 1_048_576];

    // -- Minimal roundtrip: publish 8B + recv --
    // Measures end-to-end pub/sub with negligible payload write (8 bytes).
    {
        let mut group = c.benchmark_group("pubsub_transport_only");
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
        let mut group = c.benchmark_group("pubsub_o1");
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
        let mut group = c.benchmark_group("throughput_pubsub");
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
        let mut group = c.benchmark_group("throughput_pubsub_1mb");
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
// iceoryx2 vs crossbar -- head-to-head pub/sub comparison
// ====================================================
//
// Apples-to-apples: both loan a SHM buffer, memcpy payload in, publish,
// receive (zero-copy read), deref. Same payload sizes, same pattern.

#[cfg(unix)]
fn bench_iceoryx2_vs_crossbar(c: &mut Criterion) {
    use iceoryx2::prelude::*;

    let sizes: &[(&str, usize)] = &[
        ("8B", 8),
        ("1KB", 1024),
        ("64KB", 65_536),
        ("256KB", 262_144),
        ("1MB", 1_048_576),
    ];

    // Pre-allocate payloads (filled once, reused)
    let payloads: Vec<(&str, Vec<u8>)> = sizes
        .iter()
        .map(|&(label, sz)| (label, vec![42u8; sz]))
        .collect();

    // --- iceoryx2 setup ---
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();

    // ============================================================
    // Part 1: O(1) TRANSPORT PROOF — fixed 8B write, varying buffer sizes
    // This isolates the send/receive mechanism from memcpy cost.
    // Both systems should show constant latency regardless of buffer size.
    // ============================================================
    {
        let mut group = c.benchmark_group("head_to_head_o1");
        group.measurement_time(Duration::from_secs(2));

        let o1_sizes: &[(&str, usize)] = &[
            ("64B_buf", 64),
            ("4KB_buf", 4096),
            ("64KB_buf", 65_536),
            ("256KB_buf", 262_144),
            ("1MB_buf", 1_048_576),
        ];

        // iceoryx2: backing pool has large buffers (initial_max_slice_len),
        // but we loan only 8 bytes. send() transfers a pointer — O(1).
        for &(label, buf_size) in o1_sizes {
            let svc_name: ServiceName =
                format!("bench/o1/ix2/{label}").as_str().try_into().unwrap();
            let service = node
                .service_builder(&svc_name)
                .publish_subscribe::<[u8]>()
                .enable_safe_overflow(true)
                .open_or_create()
                .unwrap();
            let publisher = service
                .publisher_builder()
                .initial_max_slice_len(buf_size)
                .create()
                .unwrap();
            let subscriber = service.subscriber_builder().create().unwrap();

            group.bench_function(format!("iceoryx2/8B_on_{label}"), |b| {
                b.iter(|| {
                    // Loan only 8 bytes — backing buffer is `buf_size` big
                    let sample = publisher.loan_slice_uninit(8).unwrap();
                    let sample = sample.write_from_fn(|_| 42u8);
                    sample.send().unwrap();
                    let recv = subscriber.receive().unwrap().unwrap();
                    black_box(recv[0]); // read 1 byte — O(1)
                })
            });
        }

        // crossbar: loan large block, write only 8 bytes, publish
        for &(label, buf_size) in o1_sizes {
            let ps_name = format!("xbar-o1-{buf_size}");
            let cfg = PubSubConfig {
                block_size: (buf_size as u32) + 64,
                block_count: 64,
                ring_depth: 8,
                ..PubSubConfig::default()
            };
            let mut pub_ = ShmPublisher::create(&ps_name, cfg).unwrap();
            let handle = pub_.register("/bench/o1").unwrap();
            let sub = ShmSubscriber::connect(&ps_name).unwrap();
            let s = sub.subscribe("/bench/o1").unwrap();

            group.bench_function(format!("crossbar/8B_on_{label}"), |b| {
                b.iter(|| {
                    let mut loan = pub_.loan(&handle);
                    loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
                    loan.set_len(8);
                    loan.publish();
                    let g = s.try_recv().unwrap();
                    black_box(g[0]); // read 1 byte — O(1)
                })
            });

            drop(pub_);
        }

        group.finish();
    }

    // ============================================================
    // Part 2: END-TO-END WITH MEMCPY — full payload write + transfer
    // Both copy the full payload into SHM. At large sizes memcpy
    // dominates, so both converge. Shows total cost for realistic use.
    // ============================================================
    {
        let mut group = c.benchmark_group("head_to_head_e2e");
        group.measurement_time(Duration::from_secs(2));

        // iceoryx2
        for &(label, ref payload) in &payloads {
            let sz = payload.len();
            let svc_name: ServiceName = format!("bench/e2e/ix2/{label}")
                .as_str()
                .try_into()
                .unwrap();
            let service = node
                .service_builder(&svc_name)
                .publish_subscribe::<[u8]>()
                .enable_safe_overflow(true)
                .open_or_create()
                .unwrap();
            let publisher = service
                .publisher_builder()
                .initial_max_slice_len(sz)
                .create()
                .unwrap();
            let subscriber = service.subscriber_builder().create().unwrap();

            group.bench_function(format!("iceoryx2/{label}"), |b| {
                b.iter(|| {
                    let sample = publisher.loan_slice_uninit(sz).unwrap();
                    let sample = sample.write_from_slice(payload);
                    sample.send().unwrap();
                    let recv = subscriber.receive().unwrap().unwrap();
                    black_box(&*recv);
                })
            });
        }

        // crossbar
        let ps_name = "crossbar-bench-h2h";
        let cfg = PubSubConfig {
            block_size: 1_048_576 + 64,
            block_count: 64,
            ring_depth: 8,
            ..PubSubConfig::default()
        };
        let mut pub_ = ShmPublisher::create(ps_name, cfg).unwrap();
        let handles: Vec<_> = sizes
            .iter()
            .map(|&(label, _)| {
                let topic = format!("/bench/{label}");
                (label, pub_.register(&topic).unwrap())
            })
            .collect();
        let sub = ShmSubscriber::connect(ps_name).unwrap();
        let subs: Vec<_> = sizes
            .iter()
            .map(|&(label, _)| {
                let topic = format!("/bench/{label}");
                (label, sub.subscribe(&topic).unwrap())
            })
            .collect();

        for (i, &(label, ref payload)) in payloads.iter().enumerate() {
            let handle = &handles[i].1;
            let sub_handle = &subs[i].1;

            group.bench_function(format!("crossbar/{label}"), |b| {
                b.iter(|| {
                    let mut loan = pub_.loan(handle);
                    loan.as_mut_slice()[..payload.len()].copy_from_slice(payload);
                    loan.set_len(payload.len());
                    loan.publish();
                    let g = sub_handle.try_recv().unwrap();
                    black_box(&*g);
                })
            });
        }

        group.finish();

        drop(pub_);
    }
}

// ====================================================

criterion_group!(benches_pubsub, bench_pubsub);

#[cfg(unix)]
criterion_group!(benches_head_to_head, bench_iceoryx2_vs_crossbar);

#[cfg(unix)]
criterion_main!(benches_pubsub, benches_head_to_head);

#[cfg(not(unix))]
criterion_main!(benches_pubsub);
