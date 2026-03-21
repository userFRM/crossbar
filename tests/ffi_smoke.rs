//! FFI smoke tests: verify null-pointer defense in extern "C" functions.
//!
//! These tests call the C FFI functions from Rust with null pointers
//! to verify that the null-defense branches return safely (no crash,
//! no UB). Requires the `ffi` feature.

use crossbar::ffi::*;
use std::ffi::CString;

// ─── crossbar_publisher_create(null, null) -> null ──────────────────────

#[test]
fn publisher_create_null_name_returns_null() {
    let result = unsafe { crossbar_publisher_create(std::ptr::null(), std::ptr::null()) };
    assert!(result.is_null(), "expected null for null name");
}

// ─── crossbar_publish(null, ...) -> -1 ──────────────────────────────────

#[test]
fn publish_null_publisher_returns_error() {
    let topic = CrossbarTopic {
        topic_idx: 0,
        publisher_id: 0,
    };
    let result = unsafe { crossbar_publish(std::ptr::null_mut(), topic, b"data".as_ptr(), 4) };
    assert_eq!(result, -1, "expected -1 for null publisher");
}

// ─── crossbar_sample_data(null) -> null ─────────────────────────────────

#[test]
fn sample_data_null_returns_null() {
    let result = unsafe { crossbar_sample_data(std::ptr::null()) };
    assert!(result.is_null(), "expected null for null sample");
}

// ─── crossbar_sample_len(null) -> 0 ────────────────────────────────────

#[test]
fn sample_len_null_returns_zero() {
    let result = unsafe { crossbar_sample_len(std::ptr::null()) };
    assert_eq!(result, 0, "expected 0 for null sample");
}

// ─── crossbar_publisher_free(null) — no crash ───────────────────────────

#[test]
fn publisher_free_null_is_safe() {
    unsafe { crossbar_publisher_free(std::ptr::null_mut()) };
    // If we reach here, the null check worked.
}

// ─── crossbar_subscriber_connect(null) -> null ──────────────────────────

#[test]
fn subscriber_connect_null_returns_null() {
    let result = unsafe { crossbar_subscriber_connect(std::ptr::null()) };
    assert!(result.is_null(), "expected null for null name");
}

// ─── crossbar_subscriber_free(null) — no crash ─────────────────────────

#[test]
fn subscriber_free_null_is_safe() {
    unsafe { crossbar_subscriber_free(std::ptr::null_mut()) };
}

// ─── crossbar_subscriber_subscribe(null, null) -> null ──────────────────

#[test]
fn subscriber_subscribe_null_returns_null() {
    let result = unsafe { crossbar_subscriber_subscribe(std::ptr::null_mut(), std::ptr::null()) };
    assert!(result.is_null());
}

// ─── crossbar_subscription_free(null) — no crash ────────────────────────

#[test]
fn subscription_free_null_is_safe() {
    unsafe { crossbar_subscription_free(std::ptr::null_mut()) };
}

// ─── crossbar_try_recv(null) -> null ────────────────────────────────────

#[test]
fn try_recv_null_returns_null() {
    let result = unsafe { crossbar_try_recv(std::ptr::null_mut()) };
    assert!(result.is_null());
}

// ─── crossbar_recv(null) -> null ────────────────────────────────────────

#[test]
fn recv_null_returns_null() {
    let result = unsafe { crossbar_recv(std::ptr::null_mut()) };
    assert!(result.is_null());
}

// ─── crossbar_sample_free(null) — no crash ──────────────────────────────

#[test]
fn sample_free_null_is_safe() {
    unsafe { crossbar_sample_free(std::ptr::null_mut()) };
}

// ─── crossbar_publisher_heartbeat(null) -> -1 ───────────────────────────

#[test]
fn publisher_heartbeat_null_returns_error() {
    let result = unsafe { crossbar_publisher_heartbeat(std::ptr::null_mut()) };
    assert_eq!(result, -1);
}

// ─── crossbar_publisher_register(null, null) -> error topic ─────────────

#[test]
fn publisher_register_null_returns_error_topic() {
    let result = unsafe { crossbar_publisher_register(std::ptr::null_mut(), std::ptr::null()) };
    assert_eq!(
        result.topic_idx,
        u32::MAX,
        "expected error topic for null publisher"
    );
}

// ─── crossbar_topic_subscriber_count(null, ...) -> 0 ────────────────────

#[test]
fn topic_subscriber_count_null_returns_zero() {
    let topic = CrossbarTopic {
        topic_idx: 0,
        publisher_id: 0,
    };
    let result = unsafe { crossbar_topic_subscriber_count(std::ptr::null_mut(), topic) };
    assert_eq!(result, 0);
}

// ─── crossbar_config_default returns valid defaults ─────────────────────

#[test]
fn config_default_valid() {
    let cfg = crossbar_config_default();
    assert_eq!(cfg.max_topics, 16);
    assert_eq!(cfg.block_count, 256);
    assert_eq!(cfg.block_size, 65536);
    assert_eq!(cfg.ring_depth, 8);
    assert_eq!(cfg.heartbeat_ms, 100);
    assert_eq!(cfg.stale_timeout_ms, 5000);
}

// ─── crossbar_channel_listen(null, ...) -> null ─────────────────────────

#[test]
fn channel_listen_null_returns_null() {
    let result = unsafe { crossbar_channel_listen(std::ptr::null(), std::ptr::null(), 1000) };
    assert!(result.is_null());
}

// ─── crossbar_channel_connect(null, ...) -> null ────────────────────────

#[test]
fn channel_connect_null_returns_null() {
    let result = unsafe { crossbar_channel_connect(std::ptr::null(), std::ptr::null(), 1000) };
    assert!(result.is_null());
}

// ─── crossbar_channel_free(null) — no crash ─────────────────────────────

#[test]
fn channel_free_null_is_safe() {
    unsafe { crossbar_channel_free(std::ptr::null_mut()) };
}

// ─── crossbar_channel_send(null, ...) -> -1 ─────────────────────────────

#[test]
fn channel_send_null_returns_error() {
    let result = unsafe { crossbar_channel_send(std::ptr::null_mut(), b"data".as_ptr(), 4) };
    assert_eq!(result, -1);
}

// ─── crossbar_channel_try_recv(null) -> null ────────────────────────────

#[test]
fn channel_try_recv_null_returns_null() {
    let result = unsafe { crossbar_channel_try_recv(std::ptr::null_mut()) };
    assert!(result.is_null());
}

// ─── crossbar_channel_recv(null) -> null ────────────────────────────────

#[test]
fn channel_recv_null_returns_null() {
    let result = unsafe { crossbar_channel_recv(std::ptr::null_mut()) };
    assert!(result.is_null());
}

// ─── crossbar_try_recv_into(null, null) -> -1 ───────────────────────────

#[test]
fn try_recv_into_null_returns_error() {
    let result = unsafe { crossbar_try_recv_into(std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(result, -1);
}

// ─── Full FFI roundtrip: create -> register -> publish -> recv -> free ──

#[test]
fn ffi_roundtrip() {
    let name = CString::new(format!("test-ffi-rt-{}", std::process::id())).unwrap();
    let uri = CString::new("/data").unwrap();

    unsafe {
        // Create publisher
        let pub_ = crossbar_publisher_create(name.as_ptr(), std::ptr::null());
        assert!(!pub_.is_null(), "publisher create should succeed");

        // Register topic
        let topic = crossbar_publisher_register(pub_, uri.as_ptr());
        let topic_idx = topic.topic_idx;
        let publisher_id = topic.publisher_id;
        assert_ne!(topic_idx, u32::MAX, "register should succeed");

        // Helper to reconstruct the topic handle (CrossbarTopic doesn't impl Copy)
        let make_topic = || CrossbarTopic {
            topic_idx,
            publisher_id,
        };

        // Publish data
        let data = b"ffi-test-data";
        let rc = crossbar_publish(pub_, make_topic(), data.as_ptr(), data.len());
        assert_eq!(rc, 0, "publish should succeed");

        // Heartbeat
        let rc = crossbar_publisher_heartbeat(pub_);
        assert_eq!(rc, 0, "heartbeat should succeed");

        // Subscriber count
        let count = crossbar_topic_subscriber_count(pub_, make_topic());
        assert_eq!(count, 0, "no subscribers yet");

        // Connect subscriber
        let sub = crossbar_subscriber_connect(name.as_ptr());
        assert!(!sub.is_null(), "subscriber connect should succeed");

        // Subscribe
        let stream = crossbar_subscriber_subscribe(sub, uri.as_ptr());
        assert!(!stream.is_null(), "subscribe should succeed");

        // Subscriber count should now be 1
        let count = crossbar_topic_subscriber_count(pub_, make_topic());
        assert_eq!(count, 1, "should have 1 subscriber");

        // Publish again so subscriber can see it (first publish was before subscribe)
        let data2 = b"ffi-data-2";
        let rc = crossbar_publish(pub_, make_topic(), data2.as_ptr(), data2.len());
        assert_eq!(rc, 0);

        // Try receive
        let sample = crossbar_try_recv(stream);
        assert!(!sample.is_null(), "should receive sample");

        let ptr = crossbar_sample_data(sample);
        assert!(!ptr.is_null());
        let len = crossbar_sample_len(sample);
        assert_eq!(len, data2.len());

        let received = std::slice::from_raw_parts(ptr, len);
        assert_eq!(received, data2);

        // Free sample
        crossbar_sample_free(sample);

        // Free subscription, subscriber, publisher
        crossbar_subscription_free(stream);
        crossbar_subscriber_free(sub);
        crossbar_publisher_free(pub_);
    }
}
