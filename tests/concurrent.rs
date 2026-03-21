use crossbar::*;
use std::sync::{Arc, Barrier};
use std::thread;

fn unique_name(suffix: &str) -> String {
    format!("test-conc-{}-{suffix}", std::process::id())
}

// ─── concurrent_publish_subscribe ───────────────────────────────────────
//
// 4 publisher threads + 4 subscriber threads on the same region.
// Each publisher publishes 1000 sequential values on its own topic.
// Subscribers verify monotonicity (no out-of-order within a topic).

#[test]
fn concurrent_publish_subscribe() {
    const NUM_PUBLISHERS: usize = 4;
    const NUM_SUBSCRIBERS: usize = 4;
    const MESSAGES_PER_PUBLISHER: u64 = 1000;

    let name = unique_name("pub-sub");
    let cfg = Config {
        max_topics: 16,
        block_count: 256,
        ring_depth: 8,
        ..Config::default()
    };

    // Create the region and register topics
    let mut creator = Publisher::create(&name, cfg).unwrap();
    let mut topic_handles = Vec::new();
    for i in 0..NUM_PUBLISHERS {
        let topic = creator.register(&format!("/topic/{i}")).unwrap();
        topic_handles.push(topic);
    }

    let barrier = Arc::new(Barrier::new(NUM_PUBLISHERS + NUM_SUBSCRIBERS));

    // Spawn subscriber threads
    let sub_handles: Vec<_> = (0..NUM_SUBSCRIBERS)
        .map(|sub_id| {
            let name = name.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let sub = Subscriber::connect(&name).unwrap();
                let mut streams = Vec::new();
                for i in 0..NUM_PUBLISHERS {
                    streams.push(sub.subscribe(&format!("/topic/{i}")).unwrap());
                }

                barrier.wait();

                // Track last seen value per topic for monotonicity check
                let mut last_seen: Vec<Option<u64>> = vec![None; NUM_PUBLISHERS];
                let mut total_received = 0u64;
                let mut done_count = 0usize;

                // Poll until all publishers have sent their final value
                let mut spin_count = 0u64;
                while done_count < NUM_PUBLISHERS {
                    let mut got_something = false;
                    for (topic_id, stream) in streams.iter().enumerate() {
                        if let Some(guard) = stream.try_recv() {
                            got_something = true;
                            let val = u64::from_le_bytes(guard[..8].try_into().unwrap());
                            // Verify monotonicity within this topic
                            if let Some(prev) = last_seen[topic_id] {
                                assert!(
                                    val > prev,
                                    "sub {sub_id}: topic {topic_id} non-monotonic: {val} after {prev}"
                                );
                            }
                            last_seen[topic_id] = Some(val);
                            total_received += 1;
                            if val == MESSAGES_PER_PUBLISHER {
                                done_count += 1;
                            }
                        }
                    }
                    if !got_something {
                        spin_count += 1;
                        if spin_count > 100_000_000 {
                            panic!("sub {sub_id}: timed out waiting for data (received {total_received})");
                        }
                        std::hint::spin_loop();
                    } else {
                        spin_count = 0;
                    }
                }

                total_received
            })
        })
        .collect();

    // Spawn publisher threads (all secondary publishers via open())
    let pub_handles: Vec<_> = (0..NUM_PUBLISHERS)
        .map(|pub_id| {
            let name = name.clone();
            let barrier = Arc::clone(&barrier);
            let _topic_handle = topic_handles[pub_id];
            thread::spawn(move || {
                let mut pub_ = Publisher::open(&name).unwrap();
                // Re-register the same topic (gets same handle via URI match)
                let topic = pub_.register(&format!("/topic/{pub_id}")).unwrap();

                barrier.wait();

                for i in 1..=MESSAGES_PER_PUBLISHER {
                    let mut loan = pub_.loan(&topic).unwrap();
                    loan.set_data(&i.to_le_bytes()).unwrap();
                    loan.publish();
                }
            })
        })
        .collect();

    // Wait for all publishers to finish
    for h in pub_handles {
        h.join().expect("publisher thread panicked");
    }

    // Wait for all subscribers to finish
    for h in sub_handles {
        let count = h.join().expect("subscriber thread panicked");
        assert!(count > 0, "subscriber received nothing");
    }

    drop(creator);
}

// ─── concurrent_multi_topic ─────────────────────────────────────────────
//
// 4 topics, 1 publisher per topic, 2 subscribers per topic.
// Each publisher publishes 500 values. Subscribers verify monotonicity.

#[test]
fn concurrent_multi_topic() {
    const NUM_TOPICS: usize = 4;
    const MSGS_PER_TOPIC: u64 = 500;
    const SUBS_PER_TOPIC: usize = 2;

    let name = unique_name("multi-topic");
    let cfg = Config {
        max_topics: 16,
        block_count: 256,
        ring_depth: 8,
        ..Config::default()
    };

    let mut creator = Publisher::create(&name, cfg).unwrap();
    for i in 0..NUM_TOPICS {
        creator.register(&format!("/mt/{i}")).unwrap();
    }

    let total_threads = NUM_TOPICS + NUM_TOPICS * SUBS_PER_TOPIC;
    let barrier = Arc::new(Barrier::new(total_threads));

    // Spawn subscriber threads: 2 per topic
    let mut sub_handles = Vec::new();
    for topic_id in 0..NUM_TOPICS {
        for sub_id in 0..SUBS_PER_TOPIC {
            let name = name.clone();
            let barrier = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                let sub = Subscriber::connect(&name).unwrap();
                let stream = sub.subscribe(&format!("/mt/{topic_id}")).unwrap();

                barrier.wait();

                let mut last: Option<u64> = None;
                let mut count = 0u64;
                let mut spin_count = 0u64;

                loop {
                    if let Some(guard) = stream.try_recv() {
                        let val = u64::from_le_bytes(guard[..8].try_into().unwrap());
                        if let Some(prev) = last {
                            assert!(
                                val > prev,
                                "topic {topic_id} sub {sub_id}: non-monotonic {val} after {prev}"
                            );
                        }
                        last = Some(val);
                        count += 1;
                        spin_count = 0;
                        if val == MSGS_PER_TOPIC {
                            break;
                        }
                    } else {
                        spin_count += 1;
                        if spin_count > 100_000_000 {
                            panic!(
                                "topic {topic_id} sub {sub_id}: timed out (got {count}, last={last:?})"
                            );
                        }
                        std::hint::spin_loop();
                    }
                }
                count
            });
            sub_handles.push(handle);
        }
    }

    // Spawn publisher threads: 1 per topic
    let mut pub_handles = Vec::new();
    for topic_id in 0..NUM_TOPICS {
        let name = name.clone();
        let barrier = Arc::clone(&barrier);
        let handle = thread::spawn(move || {
            let mut pub_ = Publisher::open(&name).unwrap();
            let topic = pub_.register(&format!("/mt/{topic_id}")).unwrap();

            barrier.wait();

            for i in 1..=MSGS_PER_TOPIC {
                let mut loan = pub_.loan(&topic).unwrap();
                loan.set_data(&i.to_le_bytes()).unwrap();
                loan.publish();
            }
        });
        pub_handles.push(handle);
    }

    for h in pub_handles {
        h.join().expect("publisher thread panicked");
    }
    for h in sub_handles {
        let count = h.join().expect("subscriber thread panicked");
        assert!(count > 0, "subscriber received nothing");
    }

    drop(creator);
}

// ─── concurrent_loan_drop_without_publish ───────────────────────────────
//
// Stress test: multiple threads loan and drop blocks without publishing.
// Verifies that the free list doesn't corrupt under concurrent access.

#[test]
fn concurrent_loan_drop_stress() {
    const NUM_THREADS: usize = 4;
    const OPS_PER_THREAD: usize = 500;

    let name = unique_name("loan-drop-stress");
    let cfg = Config {
        block_count: 32,
        ..Config::default()
    };

    let mut creator = Publisher::create(&name, cfg).unwrap();
    for i in 0..NUM_THREADS {
        creator.register(&format!("/stress/{i}")).unwrap();
    }

    let barrier = Arc::new(Barrier::new(NUM_THREADS));

    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|thread_id| {
            let name = name.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut pub_ = Publisher::open(&name).unwrap();
                let topic = pub_.register(&format!("/stress/{thread_id}")).unwrap();

                barrier.wait();

                for _ in 0..OPS_PER_THREAD {
                    // Alternate between loan+publish and loan+drop
                    if let Ok(mut loan) = pub_.loan(&topic) {
                        loan.set_data(b"stress").unwrap();
                        loan.publish();
                    }
                    if let Ok(loan) = pub_.loan(&topic) {
                        drop(loan); // return block to pool without publishing
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // If we got here without panicking, the free list survived
    // concurrent access. The region is still valid.
    drop(creator);
}
