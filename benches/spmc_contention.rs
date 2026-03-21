//! SPMC contention benchmark: proves PodBus publish cost is O(1) regardless
//! of subscriber count, and measures subscriber-side read throughput under
//! real multi-threaded contention.
//!
//! This is the benchmark that demonstrates PodBus's advantage over pool+ring:
//! - Publisher throughput doesn't degrade as subscribers are added
//! - Subscribers read independently with zero coordination cost
//! - No refcount CAS contention between subscribers

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use crossbar::{BusSubscriber, Config, PodBus, Publisher, Subscriber};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn bench_name(base: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("bench-spmc-{base}-{}-{id}", std::process::id())
}

/// Benchmark 1: Publisher throughput vs subscriber count
///
/// Measures how fast the publisher can publish as we add more subscriber
/// threads reading concurrently. If PodBus is truly O(1), publish throughput
/// should be flat.
fn publish_throughput_vs_subscriber_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("spmc_publish_throughput");
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));

    for n_subs in [0, 1, 2, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::new("podbus", format!("{n_subs}_subs")),
            &n_subs,
            |b, &n_subs| {
                let name = bench_name(&format!("pub-thru-{n_subs}"));
                let mut bus = PodBus::<u64>::create(&name, 4096).unwrap();

                // Spawn subscriber threads that spin-read
                let running = Arc::new(AtomicBool::new(true));
                let threads: Vec<_> = (0..n_subs)
                    .map(|_| {
                        let r = running.clone();
                        let n = name.clone();
                        std::thread::spawn(move || {
                            let mut sub = BusSubscriber::<u64>::connect(&n).unwrap();
                            let mut count = 0u64;
                            while r.load(Ordering::Relaxed) {
                                if sub.try_recv().is_some() {
                                    count += 1;
                                }
                                std::hint::spin_loop();
                            }
                            count
                        })
                    })
                    .collect();

                // Warmup: let subscribers connect
                std::thread::sleep(Duration::from_millis(10));

                b.iter(|| {
                    bus.publish(black_box(42u64));
                });

                running.store(false, Ordering::Relaxed);
                for t in threads {
                    let _ = t.join();
                }
            },
        );
    }

    group.finish();
}

/// Benchmark 2: Same test but with pool+ring Publisher for comparison
fn publish_throughput_pool_ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("spmc_publish_throughput");
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));

    for n_subs in [0, 1, 4, 8] {
        group.bench_with_input(
            BenchmarkId::new("pool_ring", format!("{n_subs}_subs")),
            &n_subs,
            |b, &n_subs| {
                let name = bench_name(&format!("pool-thru-{n_subs}"));
                let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
                let topic = pub_.register("/bench").unwrap();

                let running = Arc::new(AtomicBool::new(true));
                let threads: Vec<_> = (0..n_subs)
                    .map(|_| {
                        let r = running.clone();
                        let n = name.clone();
                        std::thread::spawn(move || {
                            let sub = Subscriber::connect(&n).unwrap();
                            let stream = sub.subscribe("/bench").unwrap();
                            let mut count = 0u64;
                            while r.load(Ordering::Relaxed) {
                                if stream.try_recv().is_some() {
                                    count += 1;
                                }
                                std::hint::spin_loop();
                            }
                            count
                        })
                    })
                    .collect();

                std::thread::sleep(Duration::from_millis(10));

                b.iter(|| {
                    let mut loan = pub_.loan(&topic).unwrap();
                    loan.set_data(&42u64.to_le_bytes()).unwrap();
                    loan.publish();
                });

                running.store(false, Ordering::Relaxed);
                for t in threads {
                    let _ = t.join();
                }
            },
        );
    }

    group.finish();
}

/// Benchmark 3: Total system throughput
///
/// Publisher publishes N messages, M subscriber threads each try to read all
/// of them. Measures wall-clock time for the full fanout to complete.
fn total_fanout_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("spmc_total_fanout");
    group.measurement_time(Duration::from_secs(5));

    let n_messages = 100_000u64;

    for n_subs in [1, 4, 8, 16] {
        group.throughput(Throughput::Elements(n_messages * n_subs as u64));

        group.bench_with_input(
            BenchmarkId::new("podbus", format!("{n_subs}_subs")),
            &n_subs,
            |b, &n_subs| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let name = bench_name("fanout-total");
                        let mut bus = PodBus::<u64>::create(&name, 4096).unwrap();

                        // Pre-create subscribers
                        let barrier = Arc::new(Barrier::new(n_subs + 1));
                        let done = Arc::new(AtomicBool::new(false));
                        let received = Arc::new(AtomicU64::new(0));

                        let threads: Vec<_> = (0..n_subs)
                            .map(|_| {
                                let b = barrier.clone();
                                let d = done.clone();
                                let r = received.clone();
                                let n = name.clone();
                                std::thread::spawn(move || {
                                    let mut sub = BusSubscriber::<u64>::connect(&n).unwrap();
                                    b.wait(); // sync start
                                    let mut count = 0u64;
                                    while !d.load(Ordering::Relaxed) || sub.try_recv().is_some() {
                                        if sub.try_recv().is_some() {
                                            count += 1;
                                        }
                                        std::hint::spin_loop();
                                    }
                                    r.fetch_add(count, Ordering::Relaxed);
                                })
                            })
                            .collect();

                        barrier.wait(); // all subscribers ready
                        let start = Instant::now();

                        for i in 0..n_messages {
                            bus.publish(i);
                        }

                        done.store(true, Ordering::Relaxed);
                        for t in threads {
                            t.join().unwrap();
                        }

                        total += start.elapsed();
                    }
                    total
                });
            },
        );
    }

    group.finish();
}

/// Benchmark 4: Per-subscriber read latency under contention
///
/// Publisher publishes at a steady rate. Measures how long each subscriber
/// takes to read one message when N other subscribers are also reading.
fn subscriber_read_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("spmc_sub_read_latency");
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));

    for n_other_subs in [0, 1, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::new("podbus", format!("{}_others", n_other_subs)),
            &n_other_subs,
            |b, &n_other| {
                let name = bench_name(&format!("read-lat-{n_other}"));
                let mut bus = PodBus::<u64>::create(&name, 4096).unwrap();
                let mut measured_sub = bus.subscriber().unwrap();

                // Background subscribers
                let running = Arc::new(AtomicBool::new(true));
                let threads: Vec<_> = (0..n_other)
                    .map(|_| {
                        let r = running.clone();
                        let n = name.clone();
                        std::thread::spawn(move || {
                            let mut sub = BusSubscriber::<u64>::connect(&n).unwrap();
                            while r.load(Ordering::Relaxed) {
                                black_box(sub.try_recv());
                                std::hint::spin_loop();
                            }
                        })
                    })
                    .collect();

                std::thread::sleep(Duration::from_millis(10));

                b.iter(|| {
                    bus.publish(black_box(42u64));
                    black_box(measured_sub.try_recv())
                });

                running.store(false, Ordering::Relaxed);
                for t in threads {
                    t.join().unwrap();
                }
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    publish_throughput_vs_subscriber_count,
    publish_throughput_pool_ring,
    subscriber_read_latency,
    total_fanout_throughput,
);
criterion_main!(benches);
