use crossbar::{Pod, PodBus, PodBusSubscriber};

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
    let bus = PodBus::<u64>::create(&name, 8).unwrap();
    let mut sub = bus.subscriber();

    assert!(sub.try_recv().is_none());

    bus.publish(100u64);
    let val = sub.try_recv().expect("should receive");
    assert_eq!(val, 100);
    assert!(sub.try_recv().is_none());
}

#[test]
fn multiple_subscribers_same_data() {
    let name = test_name("multi");
    let bus = PodBus::<u64>::create(&name, 16).unwrap();
    let mut s1 = bus.subscriber();
    let mut s2 = bus.subscriber();
    let mut s3 = bus.subscriber();

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
    let bus = PodBus::<u64>::create(&name, 4).unwrap();
    let mut sub = bus.subscriber();

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
    let bus = PodBus::<Tick>::create(&name, 8).unwrap();
    let mut sub = bus.subscriber();

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
    let bus = PodBus::<u64>::create(&name, 8).unwrap();

    // Publish some values before connecting
    bus.publish(10);
    bus.publish(20);

    // Connect by name (simulates cross-process subscriber)
    let mut sub = PodBusSubscriber::<u64>::connect(&name).unwrap();

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
    let result = PodBusSubscriber::<u32>::connect(&name);
    assert!(result.is_err());
}

#[test]
fn concurrent() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    const N: u64 = 10_000;
    const NUM_SUBS: usize = 4;

    let name = test_name("concurrent");
    let bus = Arc::new(PodBus::<u64>::create(&name, 1024).unwrap());
    let barrier = Arc::new(Barrier::new(NUM_SUBS + 1));

    let handles: Vec<_> = (0..NUM_SUBS)
        .map(|_| {
            let bus = Arc::clone(&bus);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut sub = bus.subscriber();
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
                            if bus.published_count() >= N && last == Some(N) {
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
        bus.publish(i);
    }

    for h in handles {
        let count = h.join().expect("subscriber thread panicked");
        // Each subscriber should have received some values (may miss some due to ring overwrite).
        assert!(count > 0, "subscriber received nothing");
    }
}
