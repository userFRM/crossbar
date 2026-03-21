use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use crossbar::{Pod, PodBus};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes64([u8; 64]);
unsafe impl Pod for Bytes64 {}

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes256([u8; 256]);
unsafe impl Pod for Bytes256 {}

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes1024([u8; 1024]);
unsafe impl Pod for Bytes1024 {}

/// Generate a unique bench name to avoid SHM file collisions.
fn bench_name(base: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("bench-{base}-{}-{id}", std::process::id())
}

fn pod_bus_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("pod_bus_latency");
    group.measurement_time(Duration::from_secs(3));

    // 8 bytes (u64)
    {
        group.throughput(Throughput::Bytes(8));
        let name = bench_name("u64");
        let mut bus = PodBus::<u64>::create(&name, 1024).unwrap();
        let mut sub = bus.subscriber().unwrap();
        group.bench_function("8B", |b| {
            b.iter(|| {
                bus.publish(black_box(42u64));
                black_box(sub.try_recv());
            });
        });
    }

    // 64 bytes
    {
        group.throughput(Throughput::Bytes(64));
        let name = bench_name("64b");
        let mut bus = PodBus::<Bytes64>::create(&name, 1024).unwrap();
        let mut sub = bus.subscriber().unwrap();
        let val = Bytes64([0xAB; 64]);
        group.bench_function("64B", |b| {
            b.iter(|| {
                bus.publish(black_box(val));
                black_box(sub.try_recv());
            });
        });
    }

    // 256 bytes
    {
        group.throughput(Throughput::Bytes(256));
        let name = bench_name("256b");
        let mut bus = PodBus::<Bytes256>::create(&name, 1024).unwrap();
        let mut sub = bus.subscriber().unwrap();
        let val = Bytes256([0xCD; 256]);
        group.bench_function("256B", |b| {
            b.iter(|| {
                bus.publish(black_box(val));
                black_box(sub.try_recv());
            });
        });
    }

    // 1024 bytes
    {
        group.throughput(Throughput::Bytes(1024));
        let name = bench_name("1kb");
        let mut bus = PodBus::<Bytes1024>::create(&name, 1024).unwrap();
        let mut sub = bus.subscriber().unwrap();
        let val = Bytes1024([0xEF; 1024]);
        group.bench_function("1KB", |b| {
            b.iter(|| {
                bus.publish(black_box(val));
                black_box(sub.try_recv());
            });
        });
    }

    group.finish();
}

fn pod_bus_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("pod_bus_fanout");
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));

    let name = bench_name("fanout");
    let mut bus = PodBus::<u64>::create(&name, 1024).unwrap();
    let mut subs: Vec<_> = (0..10).map(|_| bus.subscriber().unwrap()).collect();

    group.bench_function("10_subs_u64", |b| {
        b.iter(|| {
            bus.publish(black_box(99u64));
            for sub in subs.iter_mut() {
                black_box(sub.try_recv());
            }
        });
    });

    group.finish();
}

criterion_group!(benches, pod_bus_latency, pod_bus_fanout);
criterion_main!(benches);
