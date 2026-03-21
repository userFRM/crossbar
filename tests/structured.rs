use crossbar::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChainHeader {
    root_hash: u64,
    expiration_count: u32,
    total_strikes: u32,
}
unsafe impl Pod for ChainHeader {}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct StrikeWire {
    strike: f64,
    bid: f64,
    ask: f64,
    volume: u32,
    _pad: u32,
}
unsafe impl Pod for StrikeWire {}

#[test]
fn structured_header_array_roundtrip() {
    let name = &format!("test-struct-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let topic = pub_.register("/chain").unwrap();
    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/chain").unwrap();

    let header = ChainHeader {
        root_hash: 12345,
        expiration_count: 2,
        total_strikes: 3,
    };
    let strikes = vec![
        StrikeWire {
            strike: 100.0,
            bid: 1.5,
            ask: 2.0,
            volume: 100,
            _pad: 0,
        },
        StrikeWire {
            strike: 105.0,
            bid: 0.8,
            ask: 1.2,
            volume: 200,
            _pad: 0,
        },
        StrikeWire {
            strike: 110.0,
            bid: 0.3,
            ask: 0.6,
            volume: 50,
            _pad: 0,
        },
    ];

    let mut loan = pub_.loan(&topic).unwrap();
    loan.write_structured(&header, &strikes).unwrap();
    loan.publish();

    let sample = stream.try_recv().unwrap();
    let h: &ChainHeader = sample.read_header().unwrap();
    assert_eq!(*h, header);

    let s: &[StrikeWire] = sample.read_array::<ChainHeader, StrikeWire>().unwrap();
    assert_eq!(s.len(), 3);
    assert_eq!(s[0].strike, 100.0);
    assert_eq!(s[1].bid, 0.8);
    assert_eq!(s[2].volume, 50);
}

#[test]
fn structured_empty_array() {
    let name = &format!("test-struct-empty-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let topic = pub_.register("/empty").unwrap();
    let sub = Subscriber::connect(name).unwrap();
    let stream = sub.subscribe("/empty").unwrap();

    let header = ChainHeader {
        root_hash: 99,
        expiration_count: 0,
        total_strikes: 0,
    };
    let strikes: &[StrikeWire] = &[];

    let mut loan = pub_.loan(&topic).unwrap();
    loan.write_structured(&header, strikes).unwrap();
    loan.publish();

    let sample = stream.try_recv().unwrap();
    let h: &ChainHeader = sample.read_header().unwrap();
    assert_eq!(*h, header);

    let s: &[StrikeWire] = sample.read_array::<ChainHeader, StrikeWire>().unwrap();
    assert_eq!(s.len(), 0);
}

#[test]
fn structured_too_large_returns_error() {
    let name = &format!("test-struct-large-{}", std::process::id());
    // Use a tiny block size to force DataTooLarge
    let config = Config {
        block_size: 32,
        ..Config::default()
    };
    let mut pub_ = Publisher::create(name, config).unwrap();
    let topic = pub_.register("/big").unwrap();

    let header = ChainHeader {
        root_hash: 1,
        expiration_count: 0,
        total_strikes: 0,
    };
    // 3 StrikeWires = 3 * 32 = 96 bytes + 16 header + 4 count = 116 > 32
    let strikes = vec![
        StrikeWire {
            strike: 1.0,
            bid: 1.0,
            ask: 1.0,
            volume: 1,
            _pad: 0,
        },
        StrikeWire {
            strike: 2.0,
            bid: 2.0,
            ask: 2.0,
            volume: 2,
            _pad: 0,
        },
        StrikeWire {
            strike: 3.0,
            bid: 3.0,
            ask: 3.0,
            volume: 3,
            _pad: 0,
        },
    ];

    let mut loan = pub_.loan(&topic).unwrap();
    let result = loan.write_structured(&header, &strikes);
    assert!(result.is_err());
    match result.unwrap_err() {
        crossbar::error::Error::DataTooLarge { size, capacity } => {
            assert_eq!(
                size,
                core::mem::size_of::<ChainHeader>() + 3 * core::mem::size_of::<StrikeWire>() + 4
            );
            assert!(capacity < size);
        }
        other => panic!("expected DataTooLarge, got: {other:?}"),
    }
}
