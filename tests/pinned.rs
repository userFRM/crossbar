use crossbar::*;

fn unique_name(suffix: &str) -> String {
    format!("test-pin-{}-{suffix}", std::process::id())
}

// ─── pinned_basic ───────────────────────────────────────────────────────

#[test]
fn pinned_basic() {
    let name = unique_name("basic");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Subscribe BEFORE publishing so last_seq=0
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Loan pinned, write data, publish
    let mut loan = pub_.loan_pinned(&topic).unwrap();
    loan.set_data(b"pinned-hello").unwrap();
    loan.publish();

    // Receive via pinned path
    let guard = stream
        .try_recv_pinned()
        .expect("should receive pinned data");
    assert_eq!(&*guard, b"pinned-hello");
    assert_eq!(guard.len(), 12);
    assert!(!guard.is_empty());
}

// ─── pinned_multiple_publishes ──────────────────────────────────────────

#[test]
fn pinned_multiple_publishes() {
    let name = unique_name("multi-pub");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Publish 10 times using pinned path
    for i in 0u32..10 {
        let mut loan = pub_.loan_pinned(&topic).unwrap();
        loan.set_data(&i.to_le_bytes()).unwrap();
        loan.publish();
    }

    // Subscriber should get the latest value
    let guard = stream.try_recv_pinned().expect("should receive");
    let val = u32::from_le_bytes(guard[..4].try_into().unwrap());
    assert_eq!(val, 9, "should receive latest value");
}

// ─── pinned_guard_blocks_publisher ──────────────────────────────────────

#[test]
fn pinned_guard_blocks_publisher() {
    let name = unique_name("guard-blocks");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Subscribe BEFORE publishing
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Initial pinned publish
    let mut loan = pub_.loan_pinned(&topic).unwrap();
    loan.set_data(b"first").unwrap();
    loan.publish();

    // Subscriber gets a PinnedSample
    let guard = stream.try_recv_pinned().expect("should receive");
    assert_eq!(&*guard, b"first");

    // Publisher cannot loan_pinned while guard is held.
    // Must consume the result fully before attempting another borrow.
    {
        let result = pub_.loan_pinned(&topic);
        assert!(
            result.is_err(),
            "loan_pinned should fail while PinnedSample is held"
        );
        let msg = match result {
            Err(e) => format!("{e}"),
            Ok(_) => panic!("expected Err"),
        };
        assert!(msg.contains("PinnedSample"));
    }

    // Drop the guard — publisher should succeed now
    drop(guard);
    let mut loan2 = pub_.loan_pinned(&topic).unwrap();
    loan2.set_data(b"second").unwrap();
    loan2.publish();

    let guard2 = stream
        .try_recv_pinned()
        .expect("should receive after guard drop");
    assert_eq!(&*guard2, b"second");
}

// ─── pinned_drop_clears_sentinel ────────────────────────────────────────

#[test]
fn pinned_drop_clears_sentinel() {
    let name = unique_name("drop-sentinel");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Subscribe BEFORE any publishes
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Take a pinned loan and drop without publishing
    let loan = pub_.loan_pinned(&topic).unwrap();
    drop(loan);

    // Next loan_pinned should succeed (writer sentinel was cleared on drop)
    let mut loan2 = pub_.loan_pinned(&topic).unwrap();
    loan2.set_data(b"after-drop").unwrap();
    loan2.publish();

    let guard = stream.try_recv_pinned().expect("should receive");
    assert_eq!(&*guard, b"after-drop");
}

// ─── pinned_as_mut_slice ────────────────────────────────────────────────

#[test]
fn pinned_as_mut_slice() {
    let name = unique_name("mut-slice");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan_pinned(&topic).unwrap();
    let slice = loan.as_mut_slice();
    slice[..5].copy_from_slice(b"hello");
    loan.set_len(5).unwrap();
    loan.publish();

    let guard = stream.try_recv_pinned().expect("should receive");
    assert_eq!(&*guard, b"hello");
}

// ─── pinned capacity ────────────────────────────────────────────────────

#[test]
fn pinned_capacity_matches_config() {
    let name = unique_name("capacity");
    let cfg = Config {
        block_size: 256,
        ..Config::default()
    };
    let mut pub_ = Publisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let loan = pub_.loan_pinned(&topic).unwrap();
    // block_size=256, BLOCK_DATA_OFFSET=8, so capacity=248
    assert_eq!(loan.capacity(), 248);
}

// ─── pinned_data_too_large ──────────────────────────────────────────────

#[test]
fn pinned_data_too_large() {
    let name = unique_name("data-too-large");
    let cfg = Config {
        block_size: 16,
        ..Config::default()
    };
    let mut pub_ = Publisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let mut loan = pub_.loan_pinned(&topic).unwrap();
    // Capacity is 8 bytes (16 - 8), try to write 9
    let result = loan.set_data(&[0u8; 9]);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("exceeds block data capacity"));
}

// ─── pinned_set_len_too_large ───────────────────────────────────────────

#[test]
fn pinned_set_len_too_large() {
    let name = unique_name("set-len-too-large");
    let cfg = Config {
        block_size: 16,
        ..Config::default()
    };
    let mut pub_ = Publisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let mut loan = pub_.loan_pinned(&topic).unwrap();
    let result = loan.set_len(9);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("exceeds block data capacity"));
}

// ─── pinned: try_recv_pinned returns None when no data published ────────

#[test]
fn pinned_try_recv_returns_none_when_empty() {
    let name = unique_name("no-data");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    pub_.register("/data").unwrap();

    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    assert!(
        stream.try_recv_pinned().is_none(),
        "should return None when no pinned data published"
    );
}

// ─── pinned: same block reused across publishes ─────────────────────────

#[test]
fn pinned_reuses_same_block() {
    let name = unique_name("reuse-block");
    let cfg = Config {
        block_count: 2,
        ..Config::default()
    };
    let mut pub_ = Publisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Subscribe BEFORE publishing
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Publish pinned many times — should never exhaust the pool
    // because the same block is reused
    for i in 0u32..100 {
        let mut loan = pub_.loan_pinned(&topic).unwrap();
        loan.set_data(&i.to_le_bytes()).unwrap();
        loan.publish();
    }

    let guard = stream.try_recv_pinned().expect("should receive");
    let val = u32::from_le_bytes(guard[..4].try_into().unwrap());
    assert_eq!(val, 99);
}

// ─── PinnedSample: AsRef and Debug ───────────────────────────────────────

#[test]
fn pinned_guard_traits() {
    let name = unique_name("guard-traits");
    let mut pub_ = Publisher::create(&name, Config::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Subscribe BEFORE publishing
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan_pinned(&topic).unwrap();
    loan.set_data(b"test").unwrap();
    loan.publish();

    let guard = stream.try_recv_pinned().expect("should receive");

    // Test AsRef
    let slice: &[u8] = guard.as_ref();
    assert_eq!(slice, b"test");

    // Test Debug
    let dbg = format!("{guard:?}");
    assert!(dbg.contains("PinnedSample"));
}
