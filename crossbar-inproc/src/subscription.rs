// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subscriber handle: `Subscription<T>` for receiving messages from a topic.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::ring::Ring;
use crate::topic::TopicInner;

/// A subscription to a topic. Receives messages via a dedicated ring buffer.
///
/// Created by [`Bus::subscribe()`](crate::Bus::subscribe). Automatically
/// unsubscribes when dropped.
///
/// `Subscription` is `Send` but not `Clone` — each subscription has its own
/// independent ring buffer and read position.
pub struct Subscription<T> {
    ring: Arc<Ring<T>>,
    topic: Arc<TopicInner<T>>,
    id: u64,
}

impl<T: Send + Sync + 'static> Subscription<T> {
    pub(crate) fn new(ring: Arc<Ring<T>>, topic: Arc<TopicInner<T>>, id: u64) -> Self {
        Self { ring, topic, id }
    }

    /// Try to receive the next message without blocking.
    ///
    /// Returns `None` if no messages are pending.
    #[inline]
    pub fn try_recv(&self) -> Option<Arc<T>> {
        self.ring.pop()
    }

    /// Receive the next message, blocking until one is available.
    ///
    /// Uses a three-phase wait strategy:
    /// 1. **Spin** (32 iterations) — lowest latency for burst traffic
    /// 2. **Yield** (32 iterations) — gives other threads a chance
    /// 3. **Condvar wait** — parks the thread for idle periods
    pub fn recv(&self) -> Arc<T> {
        // Phase 1: spin
        for _ in 0..32 {
            if let Some(msg) = self.ring.pop() {
                return msg;
            }
            std::hint::spin_loop();
        }

        // Phase 2: yield
        for _ in 0..32 {
            if let Some(msg) = self.ring.pop() {
                return msg;
            }
            std::thread::yield_now();
        }

        // Phase 3: condvar
        self.topic.waiters.fetch_add(1, Ordering::Release);
        let result = loop {
            if let Some(msg) = self.ring.pop() {
                break msg;
            }
            let guard = self.topic.wake_mutex.lock().unwrap();
            // Double-check after acquiring lock
            if let Some(msg) = self.ring.pop() {
                break msg;
            }
            let _guard = self.topic.wake_condvar.wait(guard).unwrap();
        };
        self.topic.waiters.fetch_sub(1, Ordering::Release);
        result
    }

    /// Number of messages currently pending in this subscription's ring.
    #[inline]
    pub fn pending(&self) -> usize {
        self.ring.len()
    }

    /// Total messages dropped due to ring overflow for this subscription.
    #[inline]
    pub fn drops(&self) -> u64 {
        self.ring.drops()
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        self.topic.remove_subscriber(self.id);
    }
}
