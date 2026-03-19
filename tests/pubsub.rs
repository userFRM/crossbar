use crossbar::*;

// ─── O(1) pub/sub tests ─────────────────────────────────────────────────

#[test]
fn pubsub_basic_publish_subscribe() {
    let name = &format!("test-ps-basic-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/test").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/test").unwrap();

    // Publish a sample
    let mut loan = pub_.loan(&topic).unwrap();
    loan.set_data(b"hello pubsub").unwrap();
    loan.publish();

    // Receive it
    let guard = stream.try_recv().expect("should receive sample");
    assert_eq!(&*guard, b"hello pubsub");
    assert_eq!(guard.len(), 12);
}

#[test]
fn pubsub_safe_deref() {
    // Verify that SampleGuard implements safe Deref (no unsafe needed by caller).
    let name = &format!("test-ps-deref-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/tick").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/tick").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
    loan.set_len(8).unwrap();
    loan.publish();

    let guard = stream.try_recv().unwrap();
    // SAFE: Deref, no unsafe needed (unlike ring-based ShmSampleRef)
    let val = u64::from_le_bytes(guard[..8].try_into().unwrap());
    assert_eq!(val, 42);
}

#[test]
fn pubsub_guard_survives_ring_overwrite() {
    // The key advantage over ring pub/sub: guard holds block alive via refcount
    let name = &format!("test-ps-survive-{}", std::process::id());
    let cfg = PubSubConfig {
        ring_depth: 4,
        block_count: 32,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Publish first sample
    let mut loan = pub_.loan(&topic).unwrap();
    loan.set_data(b"first").unwrap();
    loan.publish();

    // Read and HOLD the guard
    let guard = stream.try_recv().unwrap();
    assert_eq!(&*guard, b"first");

    // Overwrite the ring 10x (ring_depth=4, so slot 0 is overwritten)
    for i in 0u32..10 {
        let mut loan = pub_.loan(&topic).unwrap();
        loan.set_data(&i.to_le_bytes()).unwrap();
        loan.publish();
    }

    // Guard still valid! Block is alive because refcount > 0
    assert_eq!(&*guard, b"first");
    drop(guard); // now the block is freed
}

#[test]
fn pubsub_multiple_topics() {
    let name = &format!("test-ps-topics-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let t1 = pub_.register("/a").unwrap();
    let t2 = pub_.register("/b").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let s1 = sub.subscribe("/a").unwrap();
    let s2 = sub.subscribe("/b").unwrap();

    let mut loan = pub_.loan(&t1).unwrap();
    loan.set_data(b"alpha").unwrap();
    loan.publish();

    let mut loan = pub_.loan(&t2).unwrap();
    loan.set_data(b"beta").unwrap();
    loan.publish();

    assert_eq!(&*s1.try_recv().unwrap(), b"alpha");
    assert_eq!(&*s2.try_recv().unwrap(), b"beta");
    assert!(s1.try_recv().is_none());
    assert!(s2.try_recv().is_none());
}

#[test]
fn pubsub_loan_dropped_without_publish() {
    // Loan dropped without publish should free the block (no leak)
    let name = &format!("test-ps-drop-{}", std::process::id());
    let cfg = PubSubConfig {
        block_count: 8,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(name, cfg).unwrap();
    let topic = pub_.register("/test").unwrap();

    // Allocate and drop 20 loans without publishing — if blocks leak, we'd panic
    for _ in 0..20 {
        let mut loan = pub_.loan(&topic).unwrap();
        loan.set_data(b"unused").unwrap();
        drop(loan); // should free the block
    }

    // Still works — blocks were returned to pool
    let mut loan = pub_.loan(&topic).unwrap();
    loan.set_data(b"ok").unwrap();
    loan.publish();
}

#[test]
fn pubsub_born_in_shm_pattern() {
    // iceoryx-style: write directly into the mmap block
    let name = &format!("test-ps-born-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/sensor").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/sensor").unwrap();

    // Write a struct directly into SHM (born-in-SHM)
    let mut loan = pub_.loan(&topic).unwrap();
    let buf = loan.as_mut_slice();
    let ts: u64 = 1234567890;
    let value: f64 = std::f64::consts::PI;
    buf[..8].copy_from_slice(&ts.to_le_bytes());
    buf[8..16].copy_from_slice(&value.to_le_bytes());
    loan.set_len(16).unwrap();
    loan.publish();

    let guard = stream.try_recv().unwrap();
    let read_ts = u64::from_le_bytes(guard[..8].try_into().unwrap());
    let read_val = f64::from_le_bytes(guard[8..16].try_into().unwrap());
    assert_eq!(read_ts, 1234567890);
    assert!((read_val - std::f64::consts::PI).abs() < f64::EPSILON);
}

#[test]
fn pubsub_sequential_consistency() {
    let name = &format!("test-ps-seq-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/seq").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/seq").unwrap();

    // Publish 100 sequential values
    for i in 0u64..100 {
        let mut loan = pub_.loan(&topic).unwrap();
        loan.set_data(&i.to_le_bytes()).unwrap();
        loan.publish();
    }

    // Subscriber should see monotonically increasing values
    let mut last = None;
    while let Some(guard) = stream.try_recv() {
        let val = u64::from_le_bytes(guard[..8].try_into().unwrap());
        if let Some(prev) = last {
            assert!(val > prev, "expected {val} > {prev}");
        }
        last = Some(val);
    }
    assert!(last.is_some(), "should have received at least one sample");
    assert_eq!(last, Some(99));
}
