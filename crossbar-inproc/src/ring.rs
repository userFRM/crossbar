// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded SPSC ring buffer for per-subscriber message delivery.
//!
//! Each subscriber gets a dedicated ring. The publisher writes `Arc<T>` into
//! it; the subscriber reads from it. If the subscriber falls behind, the
//! oldest messages are silently dropped (lossy semantics).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A bounded, lossy, single-producer single-consumer ring buffer.
///
/// Uses `Mutex<VecDeque>` for interior mutability. The mutex is
/// effectively uncontended in SPSC usage — publisher and subscriber
/// rarely touch the ring at the exact same instant.
pub(crate) struct Ring<T> {
    buf: Mutex<VecDeque<Arc<T>>>,
    capacity: usize,
    write_count: AtomicU64,
    drop_count: AtomicU64,
}

impl<T> Ring<T> {
    /// Create a new ring with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring capacity must be > 0");
        Self {
            buf: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            write_count: AtomicU64::new(0),
            drop_count: AtomicU64::new(0),
        }
    }

    /// Push a message. If the ring is full, drop the oldest message.
    #[inline]
    pub(crate) fn push(&self, msg: Arc<T>) {
        let mut buf = self.buf.lock().unwrap();
        if buf.len() == self.capacity {
            buf.pop_front();
            self.drop_count.fetch_add(1, Ordering::Relaxed);
        }
        buf.push_back(msg);
        self.write_count.fetch_add(1, Ordering::Release);
    }

    /// Pop the next message, or return `None` if empty.
    #[inline]
    pub(crate) fn pop(&self) -> Option<Arc<T>> {
        self.buf.lock().unwrap().pop_front()
    }

    /// Number of messages currently in the ring.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }

    /// Total messages dropped due to overflow.
    #[inline]
    pub(crate) fn drops(&self) -> u64 {
        self.drop_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_basic() {
        let ring = Ring::new(4);
        ring.push(Arc::new(1));
        ring.push(Arc::new(2));
        assert_eq!(*ring.pop().unwrap(), 1);
        assert_eq!(*ring.pop().unwrap(), 2);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn overflow_drops_oldest() {
        let ring = Ring::new(2);
        ring.push(Arc::new(1));
        ring.push(Arc::new(2));
        ring.push(Arc::new(3));
        assert_eq!(ring.drops(), 1);
        assert_eq!(*ring.pop().unwrap(), 2);
        assert_eq!(*ring.pop().unwrap(), 3);
    }

    #[test]
    fn len_tracking() {
        let ring = Ring::new(4);
        assert_eq!(ring.len(), 0);
        ring.push(Arc::new(42));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    #[should_panic(expected = "ring capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ring = Ring::<u8>::new(0);
    }
}
