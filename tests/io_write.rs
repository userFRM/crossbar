use crossbar::*;
use std::io::Write;

fn unique_name(suffix: &str) -> String {
    format!("test-iow-{}-{suffix}", std::process::id())
}

// ─── write_basic: use write!() macro to write formatted data ────────────

#[test]
fn write_basic() {
    let name = unique_name("basic");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    write!(loan, "hello {}", 42).unwrap();
    loan.publish();

    let guard = stream.try_recv().expect("should receive");
    assert_eq!(&*guard, b"hello 42");
}

// ─── write_partial: write until block is full, verify WriteZero ─────────

#[test]
fn write_partial_then_write_zero() {
    let name = unique_name("partial");
    // block_size=16 means data capacity = 16 - 8 = 8 bytes
    let cfg = PubSubConfig {
        block_size: 16,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    assert_eq!(loan.capacity(), 8);

    // Write 6 bytes — should succeed
    let n = loan.write(b"abcdef").unwrap();
    assert_eq!(n, 6);

    // Write 4 more — should only write 2 (remaining capacity)
    let n = loan.write(b"ghij").unwrap();
    assert_eq!(n, 2, "should write only remaining 2 bytes");

    // Now the block is full — next write should return WriteZero
    let result = loan.write(b"x");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
}

// ─── write_accumulates_len: multiple small writes ───────────────────────

#[test]
fn write_accumulates_len() {
    let name = unique_name("accum");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();

    // Multiple small writes
    loan.write_all(b"aaa").unwrap();
    loan.write_all(b"bbb").unwrap();
    loan.write_all(b"ccc").unwrap();

    loan.publish();

    let guard = stream.try_recv().expect("should receive");
    assert_eq!(&*guard, b"aaabbbccc");
    assert_eq!(guard.len(), 9);
}

// ─── write_empty: writing zero bytes succeeds ───────────────────────────

#[test]
fn write_empty_succeeds() {
    let name = unique_name("empty");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    let n = loan.write(b"").unwrap();
    assert_eq!(n, 0);
}

// ─── write_then_set_data_overwrites ─────────────────────────────────────
// set_data always starts from offset 0, so it effectively resets the loan.

#[test]
fn write_then_set_data_overwrites() {
    let name = unique_name("overwrite");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    // Write via io::Write
    loan.write_all(b"first").unwrap();
    // Overwrite via set_data (resets from offset 0 and sets len)
    loan.set_data(b"second").unwrap();
    loan.publish();

    let guard = stream.try_recv().expect("should receive");
    // set_data writes from offset 0 and sets len, so we see "second"
    assert_eq!(&*guard, b"second");
}

// ─── flush is a no-op ───────────────────────────────────────────────────

#[test]
fn flush_is_noop() {
    let name = unique_name("flush");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    loan.flush().unwrap(); // should succeed
}

// ─── write_formatted: write! with complex formatting ────────────────────

#[test]
fn write_formatted() {
    let name = unique_name("formatted");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    write!(loan, "price={:.2},vol={}", 123.456, 789).unwrap();
    loan.publish();

    let guard = stream.try_recv().expect("should receive");
    assert_eq!(std::str::from_utf8(&guard).unwrap(), "price=123.46,vol=789");
}

// ─── write_exactly_fills_capacity ───────────────────────────────────────

#[test]
fn write_exactly_fills_capacity() {
    let name = unique_name("exact-fill");
    let cfg = PubSubConfig {
        block_size: 16,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/data").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    assert_eq!(loan.capacity(), 8);

    // Write exactly capacity bytes
    loan.write_all(b"12345678").unwrap();
    loan.publish();

    let guard = stream.try_recv().expect("should receive");
    assert_eq!(&*guard, b"12345678");
}
