// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! C FFI bindings for crossbar pool-backed zero-copy pub/sub.
//!
//! # Safety contract
//!
//! All loans (`crossbar_loan_t`) borrow from their parent publisher
//! (`crossbar_publisher_t`). The caller **must** publish or drop every loan
//! before destroying the publisher. Violating this causes undefined behaviour.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)] // FFI functions are inherently unsafe

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::time::Duration;

use crossbar::prelude::{
    PoolPubSubConfig, PoolTopicHandle, ShmPoolLoan, ShmPoolPublisher, ShmPoolSampleGuard,
    ShmPoolSubscriber, ShmPoolSubscription,
};

// ─── Thread-local error ──────────────────────────────────────────────────

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_last_error(msg: String) {
    LAST_ERROR.with(|e| {
        // Replace interior NULs so CString::new never fails.
        let sanitised = msg.replace('\0', "\\0");
        *e.borrow_mut() = CString::new(sanitised).unwrap_or_default();
    });
}

/// Returns the last error message for the calling thread, or an empty string
/// if no error has occurred. The returned pointer is valid until the next
/// FFI call on the same thread.
#[no_mangle]
pub extern "C" fn crossbar_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

// ─── Config ──────────────────────────────────────────────────────────────

/// C-compatible mirror of `PoolPubSubConfig`.
#[repr(C)]
pub struct crossbar_config_t {
    pub max_topics: u32,
    pub block_count: u32,
    pub block_size: u32,
    pub ring_depth: u32,
    /// Heartbeat interval in microseconds.
    pub heartbeat_interval_us: u64,
    /// Publisher considered dead after this many microseconds without heartbeat.
    pub stale_timeout_us: u64,
}

impl From<&crossbar_config_t> for PoolPubSubConfig {
    fn from(c: &crossbar_config_t) -> Self {
        PoolPubSubConfig {
            max_topics: c.max_topics,
            block_count: c.block_count,
            block_size: c.block_size,
            ring_depth: c.ring_depth,
            heartbeat_interval: Duration::from_micros(c.heartbeat_interval_us),
            stale_timeout: Duration::from_micros(c.stale_timeout_us),
        }
    }
}

/// Returns a `crossbar_config_t` populated with the Rust default values.
#[no_mangle]
pub extern "C" fn crossbar_config_default() -> crossbar_config_t {
    let d = PoolPubSubConfig::default();
    crossbar_config_t {
        max_topics: d.max_topics,
        block_count: d.block_count,
        block_size: d.block_size,
        ring_depth: d.ring_depth,
        heartbeat_interval_us: d.heartbeat_interval.as_micros() as u64,
        stale_timeout_us: d.stale_timeout.as_micros() as u64,
    }
}

// ─── Opaque handle types ─────────────────────────────────────────────────

/// Opaque publisher handle.
pub struct crossbar_publisher_t {
    inner: ShmPoolPublisher,
}

/// Opaque topic handle.
pub struct crossbar_topic_t {
    inner: PoolTopicHandle,
}

/// Opaque loan handle.
///
/// # Safety
///
/// The loan borrows from `ShmPoolPublisher`. We erase the lifetime via
/// `transmute` so the loan can cross the FFI boundary. The caller **must**
/// publish or drop the loan before destroying the publisher.
pub struct crossbar_loan_t {
    inner: ShmPoolLoan<'static>,
}

/// Opaque subscriber handle.
pub struct crossbar_subscriber_t {
    inner: ShmPoolSubscriber,
}

/// Opaque subscription handle.
pub struct crossbar_subscription_t {
    inner: ShmPoolSubscription,
}

/// Opaque sample guard handle.
pub struct crossbar_sample_t {
    inner: ShmPoolSampleGuard,
}

// ─── Helpers ─────────────────────────────────────────────────────────────

/// Converts a C string pointer to a `&str`, setting last_error on failure.
unsafe fn cstr_to_str<'a>(ptr: *const c_char, label: &str) -> Option<&'a str> {
    match unsafe { CStr::from_ptr(ptr) }.to_str() {
        Ok(s) => Some(s),
        Err(e) => {
            set_last_error(format!("invalid UTF-8 in {label}: {e}"));
            None
        }
    }
}

// ─── Publisher ────────────────────────────────────────────────────────────

/// Creates a new pool publisher for the named shared-memory region.
///
/// Returns `NULL` on error (check `crossbar_last_error()`).
///
/// # Safety
///
/// `name` must be a valid null-terminated C string. `config` must point to a
/// valid `crossbar_config_t` (or use `crossbar_config_default()`).
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_create(
    name: *const c_char,
    config: *const crossbar_config_t,
) -> *mut crossbar_publisher_t {
    let Some(name) = (unsafe { cstr_to_str(name, "name") }) else {
        return std::ptr::null_mut();
    };
    let cfg: PoolPubSubConfig = unsafe { &*config }.into();

    match ShmPoolPublisher::create(name, cfg) {
        Ok(pub_) => Box::into_raw(Box::new(crossbar_publisher_t { inner: pub_ })),
        Err(e) => {
            set_last_error(e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Destroys a publisher. All loans **must** have been published or dropped
/// before calling this.
///
/// # Safety
///
/// `pub_` must be a pointer previously returned by `crossbar_publisher_create`,
/// and must not have been destroyed already.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_destroy(pub_: *mut crossbar_publisher_t) {
    if !pub_.is_null() {
        drop(unsafe { Box::from_raw(pub_) });
    }
}

/// Registers a topic URI on the publisher. Returns a topic handle or `NULL`
/// on error.
///
/// # Safety
///
/// `pub_` must be a valid publisher pointer. `topic` must be a valid
/// null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_register(
    pub_: *mut crossbar_publisher_t,
    topic: *const c_char,
) -> *mut crossbar_topic_t {
    let pub_ = unsafe { &mut *pub_ };
    let Some(uri) = (unsafe { cstr_to_str(topic, "topic") }) else {
        return std::ptr::null_mut();
    };

    match pub_.inner.register(uri) {
        Ok(handle) => Box::into_raw(Box::new(crossbar_topic_t { inner: handle })),
        Err(e) => {
            set_last_error(e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Destroys a topic handle.
///
/// # Safety
///
/// `topic` must be a pointer previously returned by `crossbar_publisher_register`,
/// and must not have been destroyed already.
#[no_mangle]
pub unsafe extern "C" fn crossbar_topic_destroy(topic: *mut crossbar_topic_t) {
    if !topic.is_null() {
        drop(unsafe { Box::from_raw(topic) });
    }
}

/// Loans a block from the pool for writing. Returns `NULL` if the pool is
/// exhausted.
///
/// The returned loan **must** be consumed via `crossbar_loan_publish` or
/// freed via `crossbar_loan_drop` before the publisher is destroyed.
///
/// # Safety
///
/// `pub_` must be a valid publisher pointer. `handle` must be a valid topic
/// handle belonging to this publisher.
#[no_mangle]
pub unsafe extern "C" fn crossbar_publisher_loan(
    pub_: *mut crossbar_publisher_t,
    handle: *const crossbar_topic_t,
) -> *mut crossbar_loan_t {
    let pub_ = unsafe { &mut *pub_ };
    let handle = unsafe { &*handle };

    // loan() panics on pool exhaustion; catch it for FFI safety.
    // We use raw pointers inside the closure to avoid lifetime escaping issues.
    let pub_ptr: *mut ShmPoolPublisher = &mut pub_.inner;
    let handle_ref: &PoolTopicHandle = &handle.inner;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pub_ref = unsafe { &mut *pub_ptr };
        let loan = pub_ref.loan(handle_ref);
        // SAFETY: We erase the lifetime. The caller is responsible for
        // ensuring the publisher outlives this loan (documented contract).
        let loan_static: ShmPoolLoan<'static> = unsafe { std::mem::transmute(loan) };
        loan_static
    }));

    match result {
        Ok(loan) => Box::into_raw(Box::new(crossbar_loan_t { inner: loan })),
        Err(_) => {
            set_last_error("pool exhausted — increase block_count".to_string());
            std::ptr::null_mut()
        }
    }
}

/// Returns a mutable pointer to the loan's data buffer.
///
/// # Safety
///
/// `loan` must be a valid loan pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_loan_data(loan: *mut crossbar_loan_t) -> *mut u8 {
    let loan = unsafe { &mut *loan };
    loan.inner.as_mut_slice().as_mut_ptr()
}

/// Returns the maximum number of bytes the loan can hold.
///
/// # Safety
///
/// `loan` must be a valid loan pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_loan_capacity(loan: *const crossbar_loan_t) -> usize {
    let loan = unsafe { &*loan };
    loan.inner.capacity()
}

/// Sets the valid data length on the loan. Must not exceed capacity.
///
/// # Safety
///
/// `loan` must be a valid loan pointer. `len` must be <= capacity.
#[no_mangle]
pub unsafe extern "C" fn crossbar_loan_set_len(loan: *mut crossbar_loan_t, len: usize) {
    let loan = unsafe { &mut *loan };
    loan.inner.set_len(len);
}

/// Publishes the loan and wakes subscribers. **Consumes** the loan — do not
/// use the pointer after this call.
///
/// # Safety
///
/// `loan` must be a valid loan pointer returned by `crossbar_publisher_loan`.
/// The pointer is consumed and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn crossbar_loan_publish(loan: *mut crossbar_loan_t) {
    let loan = unsafe { Box::from_raw(loan) };
    loan.inner.publish();
}

/// Drops the loan without publishing, returning the block to the pool.
/// **Consumes** the loan.
///
/// # Safety
///
/// `loan` must be a valid loan pointer returned by `crossbar_publisher_loan`.
/// The pointer is consumed and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn crossbar_loan_drop(loan: *mut crossbar_loan_t) {
    if !loan.is_null() {
        drop(unsafe { Box::from_raw(loan) });
    }
}

// ─── Subscriber ──────────────────────────────────────────────────────────

/// Connects to an existing pool publisher region. Returns `NULL` on error.
///
/// # Safety
///
/// `name` must be a valid null-terminated C string matching the publisher's
/// region name.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscriber_connect(
    name: *const c_char,
) -> *mut crossbar_subscriber_t {
    let Some(name) = (unsafe { cstr_to_str(name, "name") }) else {
        return std::ptr::null_mut();
    };

    match ShmPoolSubscriber::connect(name) {
        Ok(sub) => Box::into_raw(Box::new(crossbar_subscriber_t { inner: sub })),
        Err(e) => {
            set_last_error(e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Destroys a subscriber.
///
/// # Safety
///
/// `sub` must be a pointer previously returned by `crossbar_subscriber_connect`,
/// and must not have been destroyed already.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscriber_destroy(sub: *mut crossbar_subscriber_t) {
    if !sub.is_null() {
        drop(unsafe { Box::from_raw(sub) });
    }
}

/// Subscribes to a topic by URI. Returns `NULL` if the topic is not found.
///
/// # Safety
///
/// `sub` must be a valid subscriber pointer. `topic` must be a valid
/// null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscriber_subscribe(
    sub: *mut crossbar_subscriber_t,
    topic: *const c_char,
) -> *mut crossbar_subscription_t {
    let sub = unsafe { &*sub };
    let Some(uri) = (unsafe { cstr_to_str(topic, "topic") }) else {
        return std::ptr::null_mut();
    };

    match sub.inner.subscribe(uri) {
        Ok(subscription) => Box::into_raw(Box::new(crossbar_subscription_t {
            inner: subscription,
        })),
        Err(e) => {
            set_last_error(e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Destroys a subscription.
///
/// # Safety
///
/// `sub` must be a pointer previously returned by `crossbar_subscriber_subscribe`,
/// and must not have been destroyed already.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscription_destroy(sub: *mut crossbar_subscription_t) {
    if !sub.is_null() {
        drop(unsafe { Box::from_raw(sub) });
    }
}

/// Non-blocking receive. Returns a sample guard or `NULL` if no new data.
///
/// # Safety
///
/// `sub` must be a valid subscription pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_subscription_try_recv(
    sub: *mut crossbar_subscription_t,
) -> *mut crossbar_sample_t {
    let sub = unsafe { &mut *sub };
    match sub.inner.try_recv() {
        Some(guard) => Box::into_raw(Box::new(crossbar_sample_t { inner: guard })),
        None => std::ptr::null_mut(),
    }
}

/// Returns a pointer to the sample's data.
///
/// # Safety
///
/// `sample` must be a valid sample pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_sample_data(sample: *const crossbar_sample_t) -> *const u8 {
    let sample = unsafe { &*sample };
    let slice: &[u8] = &sample.inner;
    slice.as_ptr()
}

/// Returns the sample's data length in bytes.
///
/// # Safety
///
/// `sample` must be a valid sample pointer.
#[no_mangle]
pub unsafe extern "C" fn crossbar_sample_len(sample: *const crossbar_sample_t) -> usize {
    let sample = unsafe { &*sample };
    sample.inner.len()
}

/// Drops a sample guard, releasing the underlying pool block.
///
/// # Safety
///
/// `sample` must be a valid sample pointer returned by
/// `crossbar_subscription_try_recv`. The pointer is consumed and must not
/// be used again.
#[no_mangle]
pub unsafe extern "C" fn crossbar_sample_drop(sample: *mut crossbar_sample_t) {
    if !sample.is_null() {
        drop(unsafe { Box::from_raw(sample) });
    }
}
