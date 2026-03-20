use crossbar::{Pod, PodBus};

#[test]
fn basic_publish_subscribe() {
    let bus = PodBus::<u64>::new(8);
    let mut sub = bus.subscriber();

    assert!(sub.try_recv(&bus).is_none());

    bus.publish(100u64);
    let val = sub.try_recv(&bus).expect("should receive");
    assert_eq!(val, 100);
    assert!(sub.try_recv(&bus).is_none());
}

#[test]
fn multiple_subscribers_same_data() {
    let bus = PodBus::<u64>::new(16);
    let mut s1 = bus.subscriber();
    let mut s2 = bus.subscriber();
    let mut s3 = bus.subscriber();

    for i in 1..=5u64 {
        bus.publish(i);
    }

    for sub in [&mut s1, &mut s2, &mut s3] {
        let mut got = Vec::new();
        while let Some(v) = sub.try_recv(&bus) {
            got.push(v);
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
    }
}

#[test]
fn ring_overwrite() {
    let bus = PodBus::<u64>::new(4);
    let mut sub = bus.subscriber();

    // Publish 20 values into a ring of size 4.
    for i in 0..20u64 {
        bus.publish(i);
    }

    // Subscriber should skip ahead; can only see the last ring_size entries.
    let mut received = Vec::new();
    while let Some(v) = sub.try_recv(&bus) {
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
    let bus = PodBus::<Tick>::new(8);
    let mut sub = bus.subscriber();

    let tick = Tick {
        price: 123.45,
        volume: 9999,
        _pad: 0,
    };
    bus.publish(tick);

    let got = sub.try_recv(&bus).expect("should receive Tick");
    assert_eq!(got.price, 123.45);
    assert_eq!(got.volume, 9999);
}

#[test]
fn concurrent() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    const N: u64 = 10_000;
    const NUM_SUBS: usize = 4;

    let bus = Arc::new(PodBus::<u64>::new(1024));
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
                    match sub.try_recv(&bus) {
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
