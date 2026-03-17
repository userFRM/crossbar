// Copyright (c) 2026 The Crossbar Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossbar_inproc::prelude::*;
use std::sync::Arc;
use std::thread;

// --- Basic pub/sub ---

#[test]
fn publish_and_try_recv() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    let sub = bus.subscribe("t");

    topic.publish(Arc::new(42));
    assert_eq!(*sub.try_recv().unwrap(), 42);
}

#[test]
fn try_recv_empty_returns_none() {
    let bus = Bus::<u64>::new();
    let _topic = bus.topic("t");
    let sub = bus.subscribe("t");

    assert!(sub.try_recv().is_none());
}

#[test]
fn message_ordering_fifo() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    let sub = bus.subscribe("t");

    for i in 0..10 {
        topic.publish(Arc::new(i));
    }
    for i in 0..10 {
        assert_eq!(*sub.try_recv().unwrap(), i);
    }
}

#[test]
fn arc_identity_preserved() {
    // With per-subscriber SPSC rings, the subscriber gets an Arc::clone
    // of the published Arc — Arc::ptr_eq should hold.
    let bus = Bus::<String>::new();
    let topic = bus.topic("t");
    let sub = bus.subscribe("t");

    let msg = Arc::new("hello".to_string());
    topic.publish(Arc::clone(&msg));

    let received = sub.try_recv().unwrap();
    assert!(Arc::ptr_eq(&received, &msg));
}

#[test]
fn publish_via_bus_convenience() {
    let bus = Bus::<u64>::new();
    let sub = bus.subscribe("t");

    bus.publish("t", Arc::new(99));
    assert_eq!(*sub.try_recv().unwrap(), 99);

    // Convenience publish to nonexistent topic is a no-op
    bus.publish("nonexistent", Arc::new(1));
}

#[test]
fn publish_to_nonexistent_topic_is_noop() {
    let bus = Bus::<u64>::new();
    bus.publish("no_such_topic", Arc::new(1)); // should not panic
}

// --- TopicHandle ---

#[test]
fn topic_handle_reuse() {
    let bus = Bus::<u64>::new();
    let t1 = bus.topic("t");
    let t2 = bus.topic("t");

    let sub = bus.subscribe("t");
    t1.publish(Arc::new(1));
    t2.publish(Arc::new(2));

    assert_eq!(*sub.try_recv().unwrap(), 1);
    assert_eq!(*sub.try_recv().unwrap(), 2);
}

#[test]
fn topic_handle_subscriber_count() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    assert_eq!(topic.subscriber_count(), 0);

    let _s1 = bus.subscribe("t");
    assert_eq!(topic.subscriber_count(), 1);

    let _s2 = bus.subscribe("t");
    assert_eq!(topic.subscriber_count(), 2);
}

#[test]
fn topic_handle_name() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("quote:stock:AAPL");
    assert_eq!(topic.name(), "quote:stock:AAPL");
}

#[test]
fn topic_handle_publish_count() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    assert_eq!(topic.publish_count(), 0);

    topic.publish(Arc::new(1));
    topic.publish(Arc::new(2));
    assert_eq!(topic.publish_count(), 2);
}

// --- Multi-subscriber ---

#[test]
fn two_subscribers_independent() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    let s1 = bus.subscribe("t");
    let s2 = bus.subscribe("t");

    topic.publish(Arc::new(42));

    assert_eq!(*s1.try_recv().unwrap(), 42);
    assert_eq!(*s2.try_recv().unwrap(), 42);

    // Each has independent read position
    assert!(s1.try_recv().is_none());
    assert!(s2.try_recv().is_none());
}

#[test]
fn ten_subscribers() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    let subs: Vec<_> = (0..10).map(|_| bus.subscribe("t")).collect();

    topic.publish(Arc::new(7));

    for sub in &subs {
        assert_eq!(*sub.try_recv().unwrap(), 7);
    }
}

#[test]
fn late_joiner_misses_prior_messages() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");

    topic.publish(Arc::new(1));
    topic.publish(Arc::new(2));

    let sub = bus.subscribe("t");
    assert!(sub.try_recv().is_none()); // missed both
}

// --- Ring overflow ---

#[test]
fn ring_overflow_drops_oldest() {
    let bus = Bus::<u64>::new();
    let sub = bus.subscribe_with_depth("t", 2);
    let topic = bus.topic("t");

    topic.publish(Arc::new(1));
    topic.publish(Arc::new(2));
    topic.publish(Arc::new(3)); // drops 1

    assert_eq!(*sub.try_recv().unwrap(), 2);
    assert_eq!(*sub.try_recv().unwrap(), 3);
    assert!(sub.try_recv().is_none());
    assert_eq!(sub.drops(), 1);
}

#[test]
fn ring_depth_1() {
    // Capacity 1 ring — each new publish overwrites the previous.
    let bus = Bus::<u64>::new();
    let sub = bus.subscribe_with_depth("t", 1);
    let topic = bus.topic("t");

    topic.publish(Arc::new(1));
    topic.publish(Arc::new(2)); // drops 1
    topic.publish(Arc::new(3)); // drops 2

    assert_eq!(*sub.try_recv().unwrap(), 3);
    assert!(sub.try_recv().is_none());
    assert_eq!(sub.drops(), 2);
}

#[test]
fn drops_counter() {
    let bus = Bus::<u64>::new();
    let sub = bus.subscribe_with_depth("t", 4);
    let topic = bus.topic("t");

    for i in 0..100 {
        topic.publish(Arc::new(i));
    }

    // Ring holds last 4 values: 96, 97, 98, 99.
    // 96 values were dropped.
    let msg = sub.try_recv().unwrap();
    assert_eq!(*msg, 96);
    assert_eq!(sub.drops(), 96);
}

#[test]
fn pending_count() {
    let bus = Bus::<u64>::new();
    let sub = bus.subscribe("t");
    let topic = bus.topic("t");

    assert_eq!(sub.pending(), 0);
    topic.publish(Arc::new(1));
    topic.publish(Arc::new(2));
    assert_eq!(sub.pending(), 2);

    sub.try_recv();
    assert_eq!(sub.pending(), 1);
}

// --- Dynamic subscribe/unsubscribe ---

#[test]
fn drop_unsubscribes() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");

    let sub = bus.subscribe("t");
    assert_eq!(topic.subscriber_count(), 1);
    drop(sub);
    assert_eq!(topic.subscriber_count(), 0);
}

#[test]
fn subscribe_during_publish() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");

    topic.publish(Arc::new(1));

    let sub = bus.subscribe("t");
    topic.publish(Arc::new(2));

    // Only sees message 2 (subscribed after message 1)
    assert_eq!(*sub.try_recv().unwrap(), 2);
    assert!(sub.try_recv().is_none());
}

#[test]
fn resubscribe_cycle() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");

    for i in 0..5 {
        let sub = bus.subscribe("t");
        topic.publish(Arc::new(i));
        assert_eq!(*sub.try_recv().unwrap(), i);
        drop(sub);
        assert_eq!(topic.subscriber_count(), 0);
    }
}

// --- Multi-topic ---

#[test]
fn independent_topics() {
    let bus = Bus::<u64>::new();
    let t1 = bus.topic("a");
    let t2 = bus.topic("b");
    let s1 = bus.subscribe("a");
    let s2 = bus.subscribe("b");

    t1.publish(Arc::new(1));
    t2.publish(Arc::new(2));

    assert_eq!(*s1.try_recv().unwrap(), 1);
    assert_eq!(*s2.try_recv().unwrap(), 2);
    assert!(s1.try_recv().is_none());
    assert!(s2.try_recv().is_none());
}

#[test]
fn fan_out_same_arc() {
    let bus = Bus::<String>::new();
    let t1 = bus.topic("specific");
    let t2 = bus.topic("all");
    let s1 = bus.subscribe("specific");
    let s2 = bus.subscribe("all");

    let msg = Arc::new("data".to_string());
    t1.publish(Arc::clone(&msg));
    t2.publish(msg);

    let r1 = s1.try_recv().unwrap();
    let r2 = s2.try_recv().unwrap();
    assert_eq!(*r1, "data");
    assert_eq!(*r2, "data");
}

#[test]
fn topic_listing() {
    let bus = Bus::<u64>::new();
    assert_eq!(bus.topic_count(), 0);

    bus.topic("a");
    bus.topic("b");
    bus.topic("c");

    assert_eq!(bus.topic_count(), 3);

    let mut topics = bus.topics();
    topics.sort();
    assert_eq!(topics, vec!["a", "b", "c"]);
}

// --- Blocking recv ---

#[test]
fn blocking_recv_cross_thread() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    let sub = bus.subscribe("t");

    let handle = thread::spawn(move || {
        let msg = sub.recv(); // blocks until message arrives
        *msg
    });

    thread::sleep(std::time::Duration::from_millis(10));
    topic.publish(Arc::new(77));

    assert_eq!(handle.join().unwrap(), 77);
}

#[test]
fn blocking_recv_immediate() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");
    let sub = bus.subscribe("t");

    topic.publish(Arc::new(42));
    assert_eq!(*sub.recv(), 42); // should return immediately
}

// --- Concurrent access ---

#[test]
fn multi_thread_publish() {
    let bus = Bus::<u64>::new();
    let sub = bus.subscribe("t");

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let bus = bus.clone();
            thread::spawn(move || {
                let topic = bus.topic("t");
                for j in 0..100 {
                    topic.publish(Arc::new(i * 100 + j));
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let mut count = 0;
    while sub.try_recv().is_some() {
        count += 1;
    }
    // With default ring_depth=64, we'll have drops. Just check we got some.
    assert!(count > 0);
}

#[test]
fn multi_thread_subscribe() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let bus = bus.clone();
            thread::spawn(move || {
                let sub = bus.subscribe("t");
                thread::sleep(std::time::Duration::from_millis(5));
                drop(sub);
            })
        })
        .collect();

    // Publish while threads are subscribing/unsubscribing
    for i in 0..20 {
        topic.publish(Arc::new(i));
        thread::sleep(std::time::Duration::from_millis(1));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(topic.subscriber_count(), 0);
}

#[test]
fn bus_clone_shares_state() {
    let bus1 = Bus::<u64>::new();
    let bus2 = bus1.clone();

    let topic = bus1.topic("shared");
    let sub = bus2.subscribe("shared");

    topic.publish(Arc::new(99));
    assert_eq!(*sub.try_recv().unwrap(), 99);
}

// --- BusConfig ---

#[test]
fn custom_ring_depth() {
    let bus = Bus::<u64>::with_config(BusConfig { ring_depth: 2 });
    let sub = bus.subscribe("t");
    let topic = bus.topic("t");

    topic.publish(Arc::new(1));
    topic.publish(Arc::new(2));
    topic.publish(Arc::new(3)); // drops 1

    assert_eq!(*sub.try_recv().unwrap(), 2);
    assert_eq!(*sub.try_recv().unwrap(), 3);
    assert_eq!(sub.drops(), 1);
}

#[test]
fn default_config() {
    let bus = Bus::<u64>::default();
    let sub = bus.subscribe("t");
    let topic = bus.topic("t");

    // Default ring_depth is 64. 64 messages fit without overflow.
    for i in 0..64 {
        topic.publish(Arc::new(i));
    }
    assert_eq!(sub.drops(), 0);
    assert_eq!(sub.pending(), 64);
}

// --- Stress ---

#[test]
fn stress_1m_messages() {
    let bus = Bus::<u64>::with_config(BusConfig { ring_depth: 1024 });
    let topic = bus.topic("t");
    let sub = bus.subscribe("t");

    let n = 1_000_000u64;
    for i in 0..n {
        topic.publish(Arc::new(i));
    }

    // Should have last 1024 messages
    let mut count = 0u64;
    while sub.try_recv().is_some() {
        count += 1;
    }
    assert_eq!(count, 1024);
    assert_eq!(sub.drops(), n - 1024);
}

#[test]
fn stress_100_topics() {
    let bus = Bus::<u64>::new();

    let topics: Vec<_> = (0..100).map(|i| bus.topic(&format!("topic:{i}"))).collect();
    let subs: Vec<_> = (0..100)
        .map(|i| bus.subscribe(&format!("topic:{i}")))
        .collect();

    for (i, topic) in topics.iter().enumerate() {
        topic.publish(Arc::new(i as u64));
    }

    for (i, sub) in subs.iter().enumerate() {
        assert_eq!(*sub.try_recv().unwrap(), i as u64);
    }
}

#[test]
fn stress_rapid_sub_unsub() {
    let bus = Bus::<u64>::new();
    let topic = bus.topic("t");

    for i in 0..1000 {
        let sub = bus.subscribe("t");
        topic.publish(Arc::new(i));
        assert_eq!(*sub.try_recv().unwrap(), i);
        drop(sub);
    }

    assert_eq!(topic.subscriber_count(), 0);
}
