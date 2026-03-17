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
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let handle = pub_.register_typed::<Tick>("/tick/AAPL").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/tick/AAPL").unwrap();

    let tick = Tick {
        price: 42.5,
        volume: 100,
        _pad: 0,
    };
    pub_.loan_typed::<Tick>(&handle).send(tick);

    let guard = stream.try_recv_typed::<Tick>().unwrap();
    assert_eq!(*guard, tick);
}

#[test]
fn typed_send_multiple() {
    let name = &format!("test-typed-multi-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let handle = pub_.register_typed::<u64>("/counter").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/counter").unwrap();

    for i in 0..10u64 {
        pub_.loan_typed::<u64>(&handle).send(i);
    }

    // Should get the latest (ring depth = 8 by default)
    let guard = stream.try_recv_typed::<u64>().unwrap();
    assert!(*guard < 10);
}

#[test]
fn typed_loan_as_mut() {
    let name = &format!("test-typed-mut-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let handle = pub_.register_typed::<Tick>("/tick").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/tick").unwrap();

    let mut loan = pub_.loan_typed::<Tick>(&handle);
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
#[should_panic(expected = "type size mismatch")]
fn typed_size_mismatch_panics() {
    let name = &format!("test-typed-mismatch-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let _handle = pub_.register_typed::<u64>("/data").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/data").unwrap();
    let _ = stream.try_recv_typed::<u32>(); // panic: 8 != 4
}

#[test]
fn untyped_api_still_works() {
    let name = &format!("test-untyped-{}", std::process::id());
    let mut pub_ = ShmPublisher::create(name, PubSubConfig::default()).unwrap();
    let handle = pub_.register("/raw").unwrap();

    let sub = ShmSubscriber::connect(name).unwrap();
    let stream = sub.subscribe("/raw").unwrap();

    let mut loan = pub_.loan(&handle);
    loan.set_data(b"hello");
    loan.publish();

    let guard = stream.try_recv().unwrap();
    assert_eq!(&*guard, b"hello");
}
