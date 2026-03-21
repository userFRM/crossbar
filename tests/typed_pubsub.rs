use crossbar::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tick {
    price: f64,
    volume: u32,
    _pad: u32,
}
unsafe impl Pod for Tick {}

#[test]
fn typed_publish_subscribe() {
    let name = &format!("test-typed-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let handle = pub_.register_typed::<Tick>("/tick/AAPL").unwrap();

    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/tick/AAPL").unwrap();

    let tick = Tick {
        price: 42.5,
        volume: 100,
        _pad: 0,
    };
    pub_.loan_typed::<Tick>(&handle).unwrap().send(tick);

    let guard = stream.try_recv_typed::<Tick>().unwrap();
    assert_eq!(*guard, tick);
}

#[test]
fn typed_send_multiple() {
    let name = &format!("test-typed-multi-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let handle = pub_.register_typed::<u64>("/counter").unwrap();

    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/counter").unwrap();

    for i in 0..10u64 {
        pub_.loan_typed::<u64>(&handle).unwrap().send(i);
    }

    // Should get the latest (ring depth = 8 by default)
    let guard = stream.try_recv_typed::<u64>().unwrap();
    assert!(
        *guard >= 2 && *guard <= 9,
        "expected value in [2,9], got {}",
        *guard
    );
}

#[test]
fn typed_loan_as_mut() {
    let name = &format!("test-typed-mut-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let handle = pub_.register_typed::<Tick>("/tick").unwrap();

    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/tick").unwrap();

    let mut loan = pub_.loan_typed::<Tick>(&handle).unwrap();
    let t = loan.as_mut();
    t.price = 99.9;
    t.volume = 500;
    t._pad = 0;
    loan.publish();

    let guard = stream.try_recv_typed::<Tick>().unwrap();
    assert_eq!(guard.price, 99.9);
    assert_eq!(guard.volume, 500);
}

#[test]
fn typed_size_mismatch_returns_none() {
    let name = &format!("test-typed-mismatch-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let handle = pub_.register_typed::<u64>("/data").unwrap();

    // Publish a u64 value so there's data available
    pub_.loan_typed::<u64>(&handle).unwrap().send(42u64);

    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/data").unwrap();
    // Mismatched type size: topic is u64 (8 bytes), we request u32 (4 bytes)
    // Should return None instead of panicking
    assert!(
        stream.try_recv_typed::<u32>().is_none(),
        "type mismatch should return None"
    );
}

#[test]
fn untyped_api_still_works() {
    let name = &format!("test-untyped-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let handle = pub_.register("/raw").unwrap();

    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/raw").unwrap();

    let mut loan = pub_.loan(&handle).unwrap();
    loan.set_data(b"hello").unwrap();
    loan.publish();

    let guard = stream.try_recv().unwrap();
    assert_eq!(&*guard, b"hello");
}
