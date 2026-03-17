// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! C FFI bindings for crossbar.
//!
//! Build with `cargo build --release --features ffi` to produce
//! `libcrossbar.so` / `libcrossbar.dylib` / `crossbar.dll`.
//! Include `include/crossbar.h` in your C/C++ project.

use alloc::boxed::Box;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Arc;
use std::time::Duration;

use crate::platform::subscription::Subscription;
use crate::platform::{ShmChannel, ShmPublisher, ShmSubscriber};
use crate::protocol::layout::BLOCK_DATA_OFFSET;
use crate::protocol::{PubSubConfig, Region};

// ---- Config ----

/// C-compatible configuration struct.
#[repr(C)]
pub struct CrossbarConfig {
    /// Maximum number of topics (default: 16).
    pub max_topics: u32,
    /// Number of blocks in the shared pool (default: 256).
    pub block_count: u32,
    /// Size of each block in bytes (default: 65536).
    pub block_size: u32,
    /// Ring depth per topic (default: 8).
    pub ring_depth: u32,
    /// Heartbeat interval in milliseconds (default: 100).
    pub heartbeat_ms: u64,
    /// Publisher stale timeout in milliseconds (default: 5000).
    pub stale_timeout_ms: u64,
}

impl From<&CrossbarConfig> for PubSubConfig {
    fn from(c: &CrossbarConfig) -> Self {
        PubSubConfig {
            max_topics: c.max_topics,
            block_count: c.block_count,
            block_size: c.block_size,
            ring_depth: c.ring_depth,
            heartbeat_interval: Duration::from_millis(c.heartbeat_ms),
            stale_timeout: Duration::from_millis(c.stale_timeout_ms),
        }
    }
}

/// Returns a default configuration.
#[no_mangle]
pub extern "C" fn crossbar_config_default() -> CrossbarConfig {
    let d = PubSubConfig::default();
    CrossbarConfig {
        max_topics: d.max_topics,
        block_count: d.block_count,
        block_size: d.block_size,
        ring_depth: d.ring_depth,
        heartbeat_ms: d.heartbeat_interval.as_millis() as u64,
        stale_timeout_ms: d.stale_timeout.as_millis() as u64,
    }
}

fn config_from_ptr(ptr: *const CrossbarConfig) -> PubSubConfig {
    if ptr.is_null() {
        PubSubConfig::default()
    } else {
        PubSubConfig::from(unsafe { &*ptr })
    }
}

// ---- Topic handle ----

/// Value type — safe to copy.
#[repr(C)]
pub struct CrossbarTopic {
    /// Topic index within the region.
    pub topic_idx: u32,
    /// Publisher identity (for handle validation).
    pub publisher_id: u64,
}

// ---- Self-owned sample (no Rust lifetime) ----

/// Opaque sample handle. Points directly into shared memory.
pub struct CrossbarSample {
    region: Arc<Region>,
    block_idx: u32,
    len: usize,
}

impl Drop for CrossbarSample {
    fn drop(&mut self) {
        use core::sync::atomic::Ordering;
        let refcount = self.region.block_refcount(self.block_idx);
        let prev = refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.region.free_block(self.block_idx);
        }
    }
}

// ---- Publisher ----

/// Creates a publisher. Returns `NULL` on error.
///
/// # Safety
///
/// `name` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_create(
    name: *const c_char,
    config: *const CrossbarConfig,
) -> *mut ShmPublisher {
    let name = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match ShmPublisher::create(name, config_from_ptr(config)) {
        Ok(p) => Box::into_raw(Box::new(p)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Frees a publisher.
///
/// # Safety
///
/// `pub_` must be a pointer returned by `crossbar_publisher_create`, or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_free(pub_: *mut ShmPublisher) {
    if !pub_.is_null() {
        drop(Box::from_raw(pub_));
    }
}

/// Registers a topic URI. Returns a topic handle with `topic_idx == u32::MAX`
/// on error.
///
/// # Safety
///
/// `pub_` must be a valid publisher pointer. `uri` must be null-terminated.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_register(
    pub_: *mut ShmPublisher,
    uri: *const c_char,
) -> CrossbarTopic {
    let pub_ = &mut *pub_;
    let uri = match CStr::from_ptr(uri).to_str() {
        Ok(s) => s,
        Err(_) => {
            return CrossbarTopic {
                topic_idx: u32::MAX,
                publisher_id: 0,
            }
        }
    };
    match pub_.register(uri) {
        Ok(h) => CrossbarTopic {
            topic_idx: h.topic_idx,
            publisher_id: h.publisher_id,
        },
        Err(_) => CrossbarTopic {
            topic_idx: u32::MAX,
            publisher_id: 0,
        },
    }
}

/// Updates the publisher heartbeat.
///
/// # Safety
///
/// `pub_` must be a valid publisher pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_heartbeat(pub_: *mut ShmPublisher) {
    (*pub_).heartbeat();
}

/// Copies `data` into a SHM block and publishes it. Returns 0 on success.
///
/// # Safety
///
/// `pub_` must be a valid publisher pointer. `data` must point to `len`
/// readable bytes. `topic` must be a handle returned by
/// `crossbar_publisher_register` on this publisher.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publish(
    pub_: *mut ShmPublisher,
    topic: CrossbarTopic,
    data: *const u8,
    len: usize,
) -> i32 {
    let pub_ = &mut *pub_;
    let handle = crate::platform::loan::TopicHandle {
        topic_idx: topic.topic_idx,
        publisher_id: topic.publisher_id,
    };
    let mut loan = pub_.loan(&handle);
    let payload = std::slice::from_raw_parts(data, len);
    loan.set_data(payload);
    loan.publish();
    0
}

// ---- Subscriber ----

/// Connects to an existing publisher region. Returns `NULL` on error.
///
/// # Safety
///
/// `name` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscriber_connect(name: *const c_char) -> *mut ShmSubscriber {
    let name = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match ShmSubscriber::connect(name) {
        Ok(s) => Box::into_raw(Box::new(s)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Frees a subscriber. All subscriptions must be freed first.
///
/// # Safety
///
/// `sub` must be a pointer returned by `crossbar_subscriber_connect`, or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscriber_free(sub: *mut ShmSubscriber) {
    if !sub.is_null() {
        drop(Box::from_raw(sub));
    }
}

/// Subscribes to a topic by URI. Returns `NULL` on error.
///
/// # Safety
///
/// `sub` must be a valid subscriber pointer. `uri` must be null-terminated.
/// The returned subscription must not outlive the subscriber.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscriber_subscribe(
    sub: *mut ShmSubscriber,
    uri: *const c_char,
) -> *mut Subscription {
    let sub = &*sub;
    let uri = match CStr::from_ptr(uri).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match sub.subscribe(uri) {
        Ok(s) => Box::into_raw(Box::new(s)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Frees a subscription.
///
/// # Safety
///
/// `stream` must be a pointer returned by `crossbar_subscriber_subscribe`,
/// or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscription_free(stream: *mut Subscription) {
    if !stream.is_null() {
        drop(Box::from_raw(stream));
    }
}

// ---- Sample receive ----

/// Converts a `SampleGuard` to a self-owned `CrossbarSample`.
fn guard_to_sample(
    region: &Arc<Region>,
    guard: crate::platform::SampleGuard<'_>,
) -> *mut CrossbarSample {
    let sample = CrossbarSample {
        region: Arc::clone(region),
        block_idx: guard.block_idx,
        len: guard.len,
    };
    // Forget the guard so it doesn't decrement refcount — we took ownership.
    core::mem::forget(guard);
    Box::into_raw(Box::new(sample))
}

/// Non-blocking receive. Returns `NULL` if no new data.
///
/// # Safety
///
/// `stream` must be a valid subscription pointer. The returned sample
/// must be freed with `crossbar_sample_free`.
#[no_mangle]
pub unsafe extern "C" fn crossbar_try_recv(stream: *mut Subscription) -> *mut CrossbarSample {
    let stream = &*stream;
    match stream.try_recv() {
        Some(guard) => guard_to_sample(&stream.region, guard),
        None => std::ptr::null_mut(),
    }
}

/// Non-blocking receive into caller-provided memory (zero allocation).
///
/// Returns 1 if a sample was written to `out`, 0 if no data available.
/// The caller must call `crossbar_sample_free` on `out` when done if this
/// returns 1, or reuse `out` for the next call.
///
/// # Safety
///
/// `stream` must be a valid subscription pointer. `out` must point to a
/// valid `CrossbarSample` struct (may be uninitialized).
#[no_mangle]
pub unsafe extern "C" fn crossbar_try_recv_into(
    stream: *mut Subscription,
    out: *mut CrossbarSample,
) -> i32 {
    let stream = &*stream;
    match stream.try_recv() {
        Some(guard) => {
            out.write(CrossbarSample {
                region: Arc::clone(&stream.region),
                block_idx: guard.block_idx,
                len: guard.len,
            });
            core::mem::forget(guard);
            1
        }
        None => 0,
    }
}

/// Blocking receive. Returns `NULL` on error (publisher dead).
///
/// # Safety
///
/// `stream` must be a valid subscription pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_recv(stream: *mut Subscription) -> *mut CrossbarSample {
    let stream = &*stream;
    match stream.recv() {
        Ok(guard) => guard_to_sample(&stream.region, guard),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Returns a pointer to the sample's data (zero-copy — points into SHM).
///
/// # Safety
///
/// `sample` must be a valid sample pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_sample_data(sample: *const CrossbarSample) -> *const u8 {
    let s = &*sample;
    s.region.block_ptr(s.block_idx).add(BLOCK_DATA_OFFSET)
}

/// Returns the sample's data length in bytes.
///
/// # Safety
///
/// `sample` must be a valid sample pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_sample_len(sample: *const CrossbarSample) -> usize {
    (*sample).len
}

/// Frees a sample (decrements refcount, returns block to pool if last ref).
///
/// # Safety
///
/// `sample` must be a pointer returned by `crossbar_try_recv` or
/// `crossbar_recv`, or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn crossbar_sample_free(sample: *mut CrossbarSample) {
    if !sample.is_null() {
        drop(Box::from_raw(sample));
    }
}

// ---- Channel (bidirectional) ----

/// Creates the server side of a bidirectional channel. Blocks up to
/// `timeout_ms` waiting for the client. Returns `NULL` on error.
///
/// # Safety
///
/// `name` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn crossbar_channel_listen(
    name: *const c_char,
    config: *const CrossbarConfig,
    timeout_ms: u64,
) -> *mut ShmChannel {
    let name = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match ShmChannel::listen(
        name,
        config_from_ptr(config),
        Duration::from_millis(timeout_ms),
    ) {
        Ok(ch) => Box::into_raw(Box::new(ch)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Creates the client side of a bidirectional channel. Retries up to
/// `timeout_ms` waiting for the server. Returns `NULL` on error.
///
/// # Safety
///
/// `name` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn crossbar_channel_connect(
    name: *const c_char,
    config: *const CrossbarConfig,
    timeout_ms: u64,
) -> *mut ShmChannel {
    let name = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match ShmChannel::connect(
        name,
        config_from_ptr(config),
        Duration::from_millis(timeout_ms),
    ) {
        Ok(ch) => Box::into_raw(Box::new(ch)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Frees a channel.
///
/// # Safety
///
/// `ch` must be a pointer returned by `crossbar_channel_listen` or
/// `crossbar_channel_connect`, or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn crossbar_channel_free(ch: *mut ShmChannel) {
    if !ch.is_null() {
        drop(Box::from_raw(ch));
    }
}

/// Sends data through a channel. Returns 0 on success.
///
/// # Safety
///
/// `ch` must be a valid channel pointer. `data` must point to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn crossbar_channel_send(
    ch: *mut ShmChannel,
    data: *const u8,
    len: usize,
) -> i32 {
    let ch = &mut *ch;
    let payload = std::slice::from_raw_parts(data, len);
    ch.send(payload);
    0
}

/// Non-blocking receive on a channel. Returns `NULL` if no data.
///
/// # Safety
///
/// `ch` must be a valid channel pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_channel_try_recv(ch: *mut ShmChannel) -> *mut CrossbarSample {
    let ch = &*ch;
    match ch.try_recv() {
        Some(guard) => {
            // Channel's subscription holds the Arc<Region>. We need to access
            // it to create a self-owned sample. Access via the guard's region ref.
            let sample = CrossbarSample {
                region: Arc::clone(&ch.rx.region),
                block_idx: guard.block_idx,
                len: guard.len,
            };
            core::mem::forget(guard);
            Box::into_raw(Box::new(sample))
        }
        None => std::ptr::null_mut(),
    }
}

/// Blocking receive on a channel. Returns `NULL` on error.
///
/// # Safety
///
/// `ch` must be a valid channel pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_channel_recv(ch: *mut ShmChannel) -> *mut CrossbarSample {
    let ch = &*ch;
    match ch.recv() {
        Ok(guard) => {
            let sample = CrossbarSample {
                region: Arc::clone(&ch.rx.region),
                block_idx: guard.block_idx,
                len: guard.len,
            };
            core::mem::forget(guard);
            Box::into_raw(Box::new(sample))
        }
        Err(_) => std::ptr::null_mut(),
    }
}
