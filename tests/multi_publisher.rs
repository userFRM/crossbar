use crossbar::*;
use std::time::Duration;

fn unique_name(suffix: &str) -> String {
    format!("test-mp-{}-{suffix}", std::process::id())
}

#[test]
fn multi_publisher_same_topic() {
    let name = unique_name("same-topic");
    let config = Config::default();

    // Publisher A creates the region
    let mut pub_a = Publisher::create(&name, config).unwrap();
    let topic_a = pub_a.register("/data").unwrap();

    // Publisher B opens the same region
    let mut pub_b = Publisher::open(&name).unwrap();
    let topic_b = pub_b.register("/data").unwrap();

    // Subscriber
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Both publish
    let mut loan_a = pub_a.loan(&topic_a).unwrap();
    loan_a.set_data(b"from_a").unwrap();
    loan_a.publish();

    let mut loan_b = pub_b.loan(&topic_b).unwrap();
    loan_b.set_data(b"from_b").unwrap();
    loan_b.publish();

    // Subscriber should receive both (order not guaranteed)
    let mut received = Vec::new();
    for _ in 0..10 {
        if let Some(guard) = stream.try_recv() {
            received.push(guard.to_vec());
        }
        if received.len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(received.len(), 2, "should receive both messages");
    assert!(received.contains(&b"from_a".to_vec()));
    assert!(received.contains(&b"from_b".to_vec()));
}

#[test]
fn multi_publisher_different_topics() {
    let name = unique_name("diff-topics");
    let config = Config::default();

    let mut pub_a = Publisher::create(&name, config).unwrap();
    let topic_a = pub_a.register("/prices/AAPL").unwrap();

    let mut pub_b = Publisher::open(&name).unwrap();
    let topic_b = pub_b.register("/prices/GOOG").unwrap();

    let sub = Subscriber::connect(&name).unwrap();
    let stream_a = sub.subscribe("/prices/AAPL").unwrap();
    let stream_b = sub.subscribe("/prices/GOOG").unwrap();

    let mut loan = pub_a.loan(&topic_a).unwrap();
    loan.set_data(b"AAPL").unwrap();
    loan.publish();

    let mut loan = pub_b.loan(&topic_b).unwrap();
    loan.set_data(b"GOOG").unwrap();
    loan.publish();

    let guard_a = stream_a.try_recv().unwrap();
    assert_eq!(&*guard_a, b"AAPL");

    let guard_b = stream_b.try_recv().unwrap();
    assert_eq!(&*guard_b, b"GOOG");
}

#[test]
fn multi_publisher_owner_cleanup() {
    // Only the owner (creator) should delete the SHM file on drop
    let name = unique_name("owner-cleanup");
    let config = Config::default();

    let mut pub_a = Publisher::create(&name, config).unwrap();
    let _topic_a = pub_a.register("/test").unwrap();

    let mut pub_b = Publisher::open(&name).unwrap();
    let _topic_b = pub_b.register("/test").unwrap();

    // Drop non-owner first -- file should still exist
    drop(pub_b);

    // Owner can still publish
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/test").unwrap();

    let mut loan = pub_a.loan(&_topic_a).unwrap();
    loan.set_data(b"still alive").unwrap();
    loan.publish();

    let guard = stream.try_recv().unwrap();
    assert_eq!(&*guard, b"still alive");
}

#[test]
fn multi_publisher_heartbeat() {
    // Both publishers should be able to update heartbeat
    let name = unique_name("heartbeat");
    let config = Config::default();

    let mut pub_a = Publisher::create(&name, config).unwrap();
    let _topic_a = pub_a.register("/hb").unwrap();

    let mut pub_b = Publisher::open(&name).unwrap();
    let _topic_b = pub_b.register("/hb").unwrap();

    // Both update heartbeat -- should not error
    pub_a.heartbeat().unwrap();
    pub_b.heartbeat().unwrap();

    // Subscriber should still see the region as alive
    let sub = Subscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/hb").unwrap();
    assert!(stream.try_recv().is_none(), "should have no pending data");
}
