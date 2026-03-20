use crossbar::*;

fn unique_name(suffix: &str) -> String {
    format!("test-err-{}-{suffix}", std::process::id())
}

/// Helper: extract the error message from a Result that is expected to be Err.
/// Avoids calling unwrap_err() which requires T: Debug.
fn err_msg<T>(result: Result<T, impl std::fmt::Display>) -> String {
    match result {
        Err(e) => format!("{e}"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

// ─── PoolExhausted ──────────────────────────────────────────────────────

#[test]
fn pool_exhausted_when_all_blocks_loaned() {
    // Create with block_count=1, loan the only block, then try to loan again.
    let name = unique_name("pool-exhaust");
    let cfg = PubSubConfig {
        block_count: 1,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let _topic = pub_.register("/test").unwrap();

    // With block_count=1 and ring_depth=1, publish once.
    // The block stays in the ring (refcount=1). Then have a subscriber
    // hold the SampleGuard so the block can't be recycled.
    let cfg_small = PubSubConfig {
        block_count: 1,
        ring_depth: 1,
        ..PubSubConfig::default()
    };
    // Recreate with the updated config to ensure ring_depth=1
    drop(pub_);
    let mut pub_ = ShmPublisher::create(&name, cfg_small).unwrap();
    let topic = pub_.register("/test").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/test").unwrap();

    // Publish once — block goes to ring with refcount=1
    let mut loan = pub_.loan(&topic).unwrap();
    loan.set_data(b"a").unwrap();
    loan.publish();

    // Subscriber holds the block — prevents recycling
    let _guard = stream.try_recv().expect("should receive");

    // Now pool is empty AND the one block is held by the subscriber.
    // Next loan should fail with PoolExhausted.
    let msg = err_msg(pub_.loan(&topic));
    assert!(
        msg.contains("pool exhausted"),
        "expected PoolExhausted, got: {msg}"
    );

    drop(_guard);
}

// ─── DataTooLarge via set_data ──────────────────────────────────────────

#[test]
fn data_too_large_via_set_data() {
    let name = unique_name("data-too-large");
    // block_size=16 means data capacity = 16 - 8 = 8 bytes
    let cfg = PubSubConfig {
        block_size: 16,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/test").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    // Capacity is 8 bytes, try to write 9
    let msg = err_msg(loan.set_data(&[0u8; 9]));
    assert!(
        msg.contains("exceeds block data capacity"),
        "expected DataTooLarge, got: {msg}"
    );
}

// ─── DataTooLarge via set_len ───────────────────────────────────────────

#[test]
fn data_too_large_via_set_len() {
    let name = unique_name("set-len-too-large");
    let cfg = PubSubConfig {
        block_size: 16,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();
    let topic = pub_.register("/test").unwrap();

    let mut loan = pub_.loan(&topic).unwrap();
    // Capacity is 8 bytes, try to set_len(9)
    let msg = err_msg(loan.set_len(9));
    assert!(
        msg.contains("exceeds block data capacity"),
        "expected DataTooLarge, got: {msg}"
    );
}

// ─── SegmentNameInvalid: path traversal ─────────────────────────────────

#[test]
fn segment_name_invalid_dotdot() {
    let msg = err_msg(ShmPublisher::create("../evil", PubSubConfig::default()));
    assert!(
        msg.contains("invalid segment name"),
        "expected invalid segment name error, got: {msg}"
    );
}

#[test]
fn segment_name_invalid_slash() {
    let msg = err_msg(ShmPublisher::create("a/b", PubSubConfig::default()));
    assert!(
        msg.contains("invalid segment name"),
        "expected invalid segment name error, got: {msg}"
    );
}

#[test]
fn segment_name_invalid_empty() {
    let msg = err_msg(ShmPublisher::create("", PubSubConfig::default()));
    assert!(
        msg.contains("invalid segment name"),
        "expected invalid segment name error, got: {msg}"
    );
}

// ─── MaxTopicsReached ───────────────────────────────────────────────────

#[test]
fn max_topics_reached() {
    let name = unique_name("max-topics");
    let cfg = PubSubConfig {
        max_topics: 1,
        ..PubSubConfig::default()
    };
    let mut pub_ = ShmPublisher::create(&name, cfg).unwrap();

    // First topic succeeds
    pub_.register("/first").unwrap();

    // Second topic should fail
    let msg = err_msg(pub_.register("/second"));
    assert!(
        msg.contains("maximum topics reached"),
        "expected max topics error, got: {msg}"
    );
}

// ─── UriTooLong ─────────────────────────────────────────────────────────

#[test]
fn uri_too_long() {
    let name = unique_name("uri-long");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();

    // TE_URI_MAX is 64 bytes; create a 65-byte URI
    let long_uri: String = "/".to_owned() + &"x".repeat(64);
    assert_eq!(long_uri.len(), 65);

    let msg = err_msg(pub_.register(&long_uri));
    assert!(
        msg.contains("URI too long"),
        "expected URI too long error, got: {msg}"
    );
}

// ─── TopicNotFound ──────────────────────────────────────────────────────

#[test]
fn topic_not_found() {
    let name = unique_name("topic-not-found");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    pub_.register("/exists").unwrap();

    let sub = ShmSubscriber::connect(&name).unwrap();
    let msg = err_msg(sub.subscribe("/does-not-exist"));
    assert!(
        msg.contains("not found"),
        "expected topic not found error, got: {msg}"
    );
}

// ─── HandleMismatch ─────────────────────────────────────────────────────

#[test]
fn handle_mismatch_rejected() {
    let name_a = unique_name("mismatch-a");
    let name_b = unique_name("mismatch-b");

    let mut pub_a = ShmPublisher::create(&name_a, PubSubConfig::default()).unwrap();
    let topic_a = pub_a.register("/data").unwrap();

    let mut pub_b = ShmPublisher::create(&name_b, PubSubConfig::default()).unwrap();
    let _topic_b = pub_b.register("/data").unwrap();

    // Use handle from pub_a with pub_b
    let msg = err_msg(pub_b.loan(&topic_a));
    assert!(
        msg.contains("different ShmPublisher"),
        "expected handle mismatch error, got: {msg}"
    );
}

// ─── PinnedReadersActive ────────────────────────────────────────────────

#[test]
fn pinned_readers_active_blocks_loan() {
    let name = unique_name("pinned-readers");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Subscribe BEFORE publishing so the subscriber's last_seq is 0
    let sub = ShmSubscriber::connect(&name).unwrap();
    let stream = sub.subscribe("/data").unwrap();

    // Do an initial pinned publish so subscribers have something to read
    let mut loan = pub_.loan_pinned(&topic).unwrap();
    loan.set_data(b"initial").unwrap();
    loan.publish();

    // Hold a PinnedGuard
    let _guard = stream.try_recv_pinned().expect("should get pinned data");

    // Now try to loan_pinned — should fail because a reader is active
    let msg = err_msg(pub_.loan_pinned(&topic));
    assert!(
        msg.contains("PinnedGuard"),
        "expected PinnedReadersActive error, got: {msg}"
    );

    // After dropping the guard, loan_pinned should succeed again
    drop(_guard);
    let mut loan2 = pub_.loan_pinned(&topic).unwrap();
    loan2.set_data(b"ok").unwrap();
    loan2.publish();
}

// ─── AlignmentError for register_typed ──────────────────────────────────

#[test]
fn alignment_error_register_typed() {
    // Types with alignment > 8 should be rejected by register_typed.
    // BLOCK_DATA_OFFSET is 8, so align(16) types can't safely be placed.
    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    struct Aligned16 {
        _data: u128,
    }
    unsafe impl Pod for Aligned16 {}

    let name = unique_name("align-err");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();

    let msg = err_msg(pub_.register_typed::<Aligned16>("/aligned"));
    assert!(
        msg.contains("alignment"),
        "expected alignment error, got: {msg}"
    );
}

// ─── Subscriber connect to invalid segment name ─────────────────────────

#[test]
fn subscriber_connect_invalid_name() {
    let msg = err_msg(ShmSubscriber::connect("../evil"));
    assert!(
        msg.contains("invalid segment name"),
        "expected invalid segment name error, got: {msg}"
    );
}

// ─── Subscriber connect to non-existent region ──────────────────────────

#[test]
fn subscriber_connect_nonexistent_region() {
    let result = ShmSubscriber::connect("does-not-exist-at-all-42");
    assert!(result.is_err());
}

// ─── PinnedLoan dropped without publish clears sentinel ─────────────────

#[test]
fn pinned_loan_dropped_without_publish_clears_sentinel() {
    let name = unique_name("pinned-drop-sentinel");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();
    let topic = pub_.register("/data").unwrap();

    // Take a pinned loan and drop it without publishing
    let loan = pub_.loan_pinned(&topic).unwrap();
    drop(loan);

    // Should be able to loan_pinned again (sentinel was cleared)
    let mut loan2 = pub_.loan_pinned(&topic).unwrap();
    loan2.set_data(b"ok").unwrap();
    loan2.publish();
}

// ─── Handle mismatch on loan_pinned ─────────────────────────────────────

#[test]
fn handle_mismatch_on_loan_pinned() {
    let name_a = unique_name("pin-mismatch-a");
    let name_b = unique_name("pin-mismatch-b");

    let mut pub_a = ShmPublisher::create(&name_a, PubSubConfig::default()).unwrap();
    let topic_a = pub_a.register("/data").unwrap();

    let mut pub_b = ShmPublisher::create(&name_b, PubSubConfig::default()).unwrap();
    let _topic_b = pub_b.register("/data").unwrap();

    let msg = err_msg(pub_b.loan_pinned(&topic_a));
    assert!(
        msg.contains("different ShmPublisher"),
        "expected handle mismatch error, got: {msg}"
    );
}

// ─── Handle mismatch on subscriber_count ────────────────────────────────

#[test]
fn handle_mismatch_on_subscriber_count() {
    let name_a = unique_name("subcount-mismatch-a");
    let name_b = unique_name("subcount-mismatch-b");

    let mut pub_a = ShmPublisher::create(&name_a, PubSubConfig::default()).unwrap();
    let topic_a = pub_a.register("/data").unwrap();

    let pub_b = ShmPublisher::create(&name_b, PubSubConfig::default()).unwrap();

    let result = pub_b.subscriber_count(&topic_a);
    assert!(result.is_err());
}

// ─── Duplicate topic registration returns same handle ───────────────────

#[test]
fn duplicate_topic_register_returns_same_handle() {
    let name = unique_name("dup-topic");
    let mut pub_ = ShmPublisher::create(&name, PubSubConfig::default()).unwrap();

    let h1 = pub_.register("/data").unwrap();
    let h2 = pub_.register("/data").unwrap();

    assert_eq!(
        h1, h2,
        "registering same URI twice should return same handle"
    );
}

// ─── Config validation: block_size too small ────────────────────────────

#[test]
fn config_block_size_too_small() {
    let name = unique_name("block-size-small");
    let cfg = PubSubConfig {
        block_size: 8, // minimum is BLOCK_DATA_OFFSET + 1 = 9
        ..PubSubConfig::default()
    };
    let msg = err_msg(ShmPublisher::create(&name, cfg));
    assert!(
        msg.contains("block_size"),
        "expected block_size error, got: {msg}"
    );
}

// ─── Config validation: ring_depth not power of two ─────────────────────

#[test]
fn config_ring_depth_not_power_of_two() {
    let name = unique_name("ring-depth-bad");
    let cfg = PubSubConfig {
        ring_depth: 3,
        ..PubSubConfig::default()
    };
    let msg = err_msg(ShmPublisher::create(&name, cfg));
    assert!(
        msg.contains("ring_depth"),
        "expected ring_depth error, got: {msg}"
    );
}

// ─── Config validation: block_count zero ────────────────────────────────

#[test]
fn config_block_count_zero() {
    let name = unique_name("block-count-zero");
    let cfg = PubSubConfig {
        block_count: 0,
        ..PubSubConfig::default()
    };
    let msg = err_msg(ShmPublisher::create(&name, cfg));
    assert!(
        msg.contains("block_count"),
        "expected block_count error, got: {msg}"
    );
}

// ─── Config validation: max_topics zero ─────────────────────────────────

#[test]
fn config_max_topics_zero() {
    let name = unique_name("max-topics-zero");
    let cfg = PubSubConfig {
        max_topics: 0,
        ..PubSubConfig::default()
    };
    let msg = err_msg(ShmPublisher::create(&name, cfg));
    assert!(
        msg.contains("max_topics"),
        "expected max_topics error, got: {msg}"
    );
}
