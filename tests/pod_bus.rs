use crossbar::error::Error;
use crossbar::{BusSubscriber, Pod, PodBus};

/// Generate a unique test name to avoid SHM file collisions between tests.
fn test_name(base: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("itest-{base}-{}-{id}", std::process::id())
}

#[test]
fn basic_publish_subscribe() {
    let name = test_name("basic");
    let mut bus = PodBus::<u64>::create(&name, 8).unwrap();
    let mut sub = bus.subscriber().unwrap();

    assert!(sub.try_recv().is_none());

    bus.publish(100u64);
    let val = sub.try_recv().expect("should receive");
    assert_eq!(val, 100);
    assert!(sub.try_recv().is_none());
}

#[test]
fn multiple_subscribers_same_data() {
    let name = test_name("multi");
    let mut bus = PodBus::<u64>::create(&name, 16).unwrap();
    let mut s1 = bus.subscriber().unwrap();
    let mut s2 = bus.subscriber().unwrap();
    let mut s3 = bus.subscriber().unwrap();

    for i in 1..=5u64 {
        bus.publish(i);
    }

    for sub in [&mut s1, &mut s2, &mut s3] {
        let mut got = Vec::new();
        while let Some(v) = sub.try_recv() {
            got.push(v);
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
    }
}

#[test]
fn ring_overwrite() {
    let name = test_name("overwrite");
    let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
    let mut sub = bus.subscriber().unwrap();

    // Publish 20 values into a ring of size 4.
    for i in 0..20u64 {
        bus.publish(i);
    }

    // Subscriber should skip ahead; can only see the last ring_size entries.
    let mut received = Vec::new();
    while let Some(v) = sub.try_recv() {
        received.push(v);
    }

    assert!(!received.is_empty());
    assert!(received.len() <= 4);
    // Last value must be 19.
    assert_eq!(*received.last().unwrap(), 19);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tick {
    price: f64,
    volume: u32,
    _pad: u32,
}
unsafe impl Pod for Tick {}

#[test]
fn custom_pod_struct() {
    let name = test_name("pod-struct");
    let mut bus = PodBus::<Tick>::create(&name, 8).unwrap();
    let mut sub = bus.subscriber().unwrap();

    let tick = Tick {
        price: 123.45,
        volume: 9999,
        _pad: 0,
    };
    bus.publish(tick);

    let got = sub.try_recv().expect("should receive Tick");
    assert_eq!(got.price, 123.45);
    assert_eq!(got.volume, 9999);
}

#[test]
fn connect_by_name() {
    let name = test_name("connect");
    let mut bus = PodBus::<u64>::create(&name, 8).unwrap();

    // Publish some values before connecting
    bus.publish(10);
    bus.publish(20);

    // Connect by name (simulates cross-process subscriber)
    let mut sub = BusSubscriber::<u64>::connect(&name).unwrap();

    // Subscriber starts at current write position, so it only sees new values
    bus.publish(30);
    assert_eq!(sub.try_recv(), Some(30));
    assert!(sub.try_recv().is_none());
}

#[test]
fn type_mismatch_rejected() {
    let name = test_name("mismatch");
    let _bus = PodBus::<u64>::create(&name, 8).unwrap();

    // Try connecting with wrong type
    let result = BusSubscriber::<u32>::connect(&name);
    assert!(result.is_err());
}

#[test]
fn concurrent() {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    const N: u64 = 10_000;
    const NUM_SUBS: usize = 4;

    let name = test_name("concurrent");
    let bus = Arc::new(Mutex::new(PodBus::<u64>::create(&name, 1024).unwrap()));
    let barrier = Arc::new(Barrier::new(NUM_SUBS + 1));

    // Create subscribers before spawning threads (publisher is behind Mutex).
    let subs: Vec<_> = {
        let bus_guard = bus.lock().unwrap();
        (0..NUM_SUBS)
            .map(|_| bus_guard.subscriber().unwrap())
            .collect()
    };

    let handles: Vec<_> = subs
        .into_iter()
        .map(|mut sub| {
            let bus = Arc::clone(&bus);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();

                let mut last = None;
                let mut count = 0u64;
                loop {
                    match sub.try_recv() {
                        Some(v) => {
                            // Values must be monotonically increasing when received in order.
                            if let Some(prev) = last {
                                assert!(v > prev, "non-monotonic: {} after {}", v, prev);
                            }
                            last = Some(v);
                            count += 1;
                            if v == N {
                                break;
                            }
                        }
                        None => {
                            // If publisher is done and we got the last value, stop.
                            if bus.lock().unwrap().published_count() >= N && last == Some(N) {
                                break;
                            }
                            std::hint::spin_loop();
                        }
                    }
                }
                count
            })
        })
        .collect();

    // Publisher.
    barrier.wait();
    for i in 1..=N {
        bus.lock().unwrap().publish(i);
    }

    for h in handles {
        let count = h.join().expect("subscriber thread panicked");
        // Each subscriber should have received some values (may miss some due to ring overwrite).
        assert!(count > 0, "subscriber received nothing");
    }
}

#[test]
fn subscriber_survives_publisher_drop() {
    let name = test_name("survive");
    let mut sub;
    {
        let mut bus = PodBus::<u64>::create(&name, 8).unwrap();
        bus.publish(42);
        bus.publish(43);
        sub = bus.subscriber().unwrap();
        bus.publish(44);
        // bus drops here -- subscriber has its own mmap
    }
    // subscriber can still read what was published before drop
    let mut received = Vec::new();
    while let Some(v) = sub.try_recv() {
        received.push(v);
    }
    assert_eq!(received, vec![44]);
}

#[test]
fn second_create_fails_while_first_alive() {
    let name = test_name("locktest");
    let _bus = PodBus::<u64>::create(&name, 4).unwrap();

    // A second create on the same name should fail (lock contention).
    let result = PodBus::<u64>::create(&name, 4);
    assert!(result.is_err());
}

#[test]
fn lag_detection_via_total_lagged() {
    let name = test_name("lag");
    let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
    let mut sub = bus.subscriber().unwrap();

    // Publish way more than ring_size
    for i in 0..100u64 {
        bus.publish(i);
    }

    // Drain what we can
    while sub.try_recv().is_some() {}

    // total_lagged should be > 0
    assert!(sub.total_lagged() > 0, "expected lag but got 0");
}

#[test]
fn heartbeat_method_works() {
    let name = test_name("hb");
    let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
    // Should succeed without publishing anything.
    bus.heartbeat().unwrap();
}

// ---- Bounded backpressure integration tests ----

#[test]
fn bounded_backpressure_blocks() {
    let name = test_name("bp-blocks");
    // Ring of 4, watermark 1 => effective capacity = 3
    let mut bus = PodBus::<u64>::create_bounded(&name, 4, 1).unwrap();
    assert!(bus.is_bounded());

    let mut sub = bus.subscriber().unwrap();

    // Fill up to effective capacity (3 values).
    assert!(bus.try_publish(1).is_ok());
    assert!(bus.try_publish(2).is_ok());
    assert!(bus.try_publish(3).is_ok());

    // 4th should fail -- ring is full.
    let result = bus.try_publish(4);
    assert!(
        matches!(result, Err(Error::Full)),
        "expected Error::Full, got {result:?}"
    );

    // Subscriber reads one value, freeing a slot.
    assert_eq!(sub.try_recv(), Some(1));

    // Now we can publish again.
    assert!(bus.try_publish(4).is_ok());
}

#[test]
fn bounded_subscriber_advances() {
    let name = test_name("bp-adv");
    // Ring of 8, watermark 0 => effective capacity = 8
    let mut bus = PodBus::<u64>::create_bounded(&name, 8, 0).unwrap();
    let mut sub = bus.subscriber().unwrap();

    // Fill ring completely.
    for i in 0..8u64 {
        assert!(bus.try_publish(i).is_ok(), "failed to publish {i}");
    }

    // Ring is full.
    assert!(matches!(bus.try_publish(99), Err(Error::Full)));

    // Subscriber reads all values.
    let mut received = Vec::new();
    while let Some(v) = sub.try_recv() {
        received.push(v);
    }
    assert_eq!(received, vec![0, 1, 2, 3, 4, 5, 6, 7]);

    // Now we can publish again (subscriber advanced cursor).
    for i in 100..108u64 {
        assert!(bus.try_publish(i).is_ok(), "failed to publish {i}");
    }
    assert!(matches!(bus.try_publish(999), Err(Error::Full)));
}

#[test]
fn bounded_crashed_subscriber_pruned() {
    let name = test_name("bp-crash");
    // Ring of 4, watermark 0 => effective capacity = 4
    let mut bus = PodBus::<u64>::create_bounded(&name, 4, 0).unwrap();

    // Create subscriber and fill the ring.
    let sub = bus.subscriber().unwrap();
    for i in 0..4u64 {
        assert!(bus.try_publish(i).is_ok());
    }

    // Ring is full.
    assert!(matches!(bus.try_publish(99), Err(Error::Full)));

    // Drop the subscriber (simulates crash -- slot released on drop).
    drop(sub);

    // With no active subscribers, publisher can proceed.
    assert!(bus.try_publish(99).is_ok());
}

#[test]
fn bounded_publish_spin_waits() {
    use std::thread;
    use std::time::Duration;

    let name = test_name("bp-spin");
    let mut bus = PodBus::<u64>::create_bounded(&name, 4, 1).unwrap();
    let mut sub = bus.subscriber().unwrap();

    // Spawn a thread that reads after a short delay.
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        let mut count = 0;
        while count < 5 {
            if sub.try_recv().is_some() {
                count += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        count
    });

    // Publish 5 values using blocking publish (ring only holds 3).
    for i in 0..5u64 {
        bus.publish(i);
    }

    let count = handle.join().unwrap();
    assert_eq!(count, 5);
}

#[test]
fn bounded_multiple_subscribers_slowest_wins() {
    let name = test_name("bp-slow");
    // Ring of 4, watermark 0 => effective capacity = 4
    let mut bus = PodBus::<u64>::create_bounded(&name, 4, 0).unwrap();
    let mut fast_sub = bus.subscriber().unwrap();
    let _slow_sub = bus.subscriber().unwrap(); // never reads

    // Fill ring.
    for i in 0..4u64 {
        assert!(bus.try_publish(i).is_ok());
    }

    // Ring is full because slow_sub hasn't advanced.
    assert!(matches!(bus.try_publish(99), Err(Error::Full)));

    // Fast subscriber reads, but ring is still full because slow_sub holds position 0.
    assert_eq!(fast_sub.try_recv(), Some(0));
    assert!(matches!(bus.try_publish(99), Err(Error::Full)));
}

#[test]
fn bounded_no_subscribers_unbounded() {
    let name = test_name("bp-nosub");
    // With no subscribers, a bounded bus should publish freely.
    let mut bus = PodBus::<u64>::create_bounded(&name, 4, 1).unwrap();

    for i in 0..100u64 {
        assert!(bus.try_publish(i).is_ok());
    }
}

#[test]
fn unbounded_try_publish_always_succeeds() {
    let name = test_name("unbounded-try");
    let mut bus = PodBus::<u64>::create(&name, 4).unwrap();
    let _sub = bus.subscriber().unwrap();

    // Even without reading, try_publish should always succeed on an unbounded bus.
    for i in 0..100u64 {
        assert!(bus.try_publish(i).is_ok());
    }
}
