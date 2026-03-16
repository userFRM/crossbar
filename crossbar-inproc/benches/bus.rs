// Copyright (c) 2026 The Crossbar Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use criterion::{criterion_group, criterion_main, Criterion};
use crossbar_inproc::prelude::*;
use std::hint::black_box;
use std::sync::Arc;

fn bench_1_sub_roundtrip(c: &mut Criterion) {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("bench");
    let sub = bus.subscribe("bench");

    c.bench_function("1_sub_roundtrip", |b| {
        b.iter(|| {
            topic.publish(Arc::new(black_box(42u64)));
            black_box(sub.try_recv().unwrap());
        });
    });
}

fn bench_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("fanout");

    for n in [0, 1, 2, 5, 10] {
        let bus = Bus::<u64>::new();
        let topic = bus.topic("bench");
        let subs: Vec<_> = (0..n).map(|_| bus.subscribe("bench")).collect();

        group.bench_function(format!("{n}_subscribers"), |b| {
            b.iter(|| {
                topic.publish(Arc::new(black_box(42u64)));
                for sub in &subs {
                    let _ = black_box(sub.try_recv());
                }
            });
        });
    }
    group.finish();
}

fn bench_publish_only(c: &mut Criterion) {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("bench");
    let _sub = bus.subscribe("bench");

    c.bench_function("publish_only_1_sub", |b| {
        b.iter(|| {
            topic.publish(Arc::new(black_box(42u64)));
        });
    });
}

fn bench_handle_vs_dynamic(c: &mut Criterion) {
    let mut group = c.benchmark_group("handle_vs_dynamic");

    // TopicHandle (no hash lookup)
    let bus = Bus::<u64>::new();
    let topic = bus.topic("bench");
    let sub = bus.subscribe("bench");

    group.bench_function("handle", |b| {
        b.iter(|| {
            topic.publish(Arc::new(black_box(42u64)));
            black_box(sub.try_recv().unwrap());
        });
    });

    // bus.publish (hash lookup each time)
    let bus2 = Bus::<u64>::new();
    let _sub2 = bus2.subscribe("bench");

    group.bench_function("dynamic", |b| {
        b.iter(|| {
            bus2.publish("bench", Arc::new(black_box(42u64)));
        });
    });

    group.finish();
}

fn bench_arc_overhead(c: &mut Criterion) {
    c.bench_function("arc_new_clone_drop", |b| {
        b.iter(|| {
            let a = Arc::new(black_box(42u64));
            let b_clone = Arc::clone(&a);
            black_box(b_clone);
        });
    });
}

fn bench_subscribe_unsubscribe(c: &mut Criterion) {
    let bus = Bus::<u64>::new();
    bus.topic("bench"); // ensure topic exists

    c.bench_function("subscribe_unsubscribe", |b| {
        b.iter(|| {
            let sub = bus.subscribe("bench");
            black_box(&sub);
            drop(sub);
        });
    });
}

fn bench_try_recv_empty(c: &mut Criterion) {
    let bus = Bus::<u64>::new();
    let _topic = bus.topic("bench");
    let sub = bus.subscribe("bench");

    c.bench_function("try_recv_empty", |b| {
        b.iter(|| {
            black_box(sub.try_recv());
        });
    });
}

criterion_group!(
    benches,
    bench_1_sub_roundtrip,
    bench_fanout,
    bench_publish_only,
    bench_handle_vs_dynamic,
    bench_arc_overhead,
    bench_subscribe_unsubscribe,
    bench_try_recv_empty,
);
criterion_main!(benches);
