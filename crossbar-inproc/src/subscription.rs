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
use crate::wait::WaitStrategy;

/// A subscription to a topic. Receives messages from a dedicated SPSC ring.
///
/// Created by [`Bus::subscribe()`](crate::Bus::subscribe). Automatically
/// unsubscribes when dropped.
///
/// Each subscription has its own ring, so there is no contention between
/// subscribers on the read path.
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
    /// Uses the default [`WaitStrategy::Adaptive`] with three phases:
    /// 1. **Spin** (32 iterations) — lowest latency for burst traffic
    /// 2. **Yield** (32 iterations) — gives other threads a chance
    /// 3. **Condvar wait** — parks the thread for idle periods
    ///
    /// For custom wait behaviour, use [`recv_with`](Self::recv_with).
    pub fn recv(&self) -> Arc<T> {
        self.recv_with(WaitStrategy::default())
    }

    /// Receive the next message using a custom wait strategy.
    ///
    /// The strategy controls how the thread waits between checks:
    /// - [`BusySpin`](WaitStrategy::BusySpin): lowest latency, burns 100% CPU
    /// - [`YieldSpin`](WaitStrategy::YieldSpin): PAUSE/WFE hint, good for shared cores
    /// - [`BackoffSpin`](WaitStrategy::BackoffSpin): exponential backoff, reduces CPU over time
    /// - [`Adaptive`](WaitStrategy::Adaptive): spin -> yield -> condvar (default, most balanced)
    pub fn recv_with(&self, strategy: WaitStrategy) -> Arc<T> {
        let mut iter: u32 = 0;

        loop {
            if let Some(msg) = self.try_recv() {
                return msg;
            }

            // For Adaptive strategy, transition to condvar after spin+yield phases
            if let WaitStrategy::Adaptive {
                spin_iters,
                yield_iters,
            } = strategy
            {
                if iter >= spin_iters + yield_iters {
                    // Phase 3: condvar (OS sleep)
                    self.topic.waiters.fetch_add(1, Ordering::Release);
                    let result = loop {
                        if let Some(msg) = self.try_recv() {
                            break msg;
                        }
                        let guard = self.topic.wake_mutex.lock().unwrap();
                        // Double-check after acquiring lock
                        if let Some(msg) = self.try_recv() {
                            break msg;
                        }
                        let _guard = self.topic.wake_condvar.wait(guard).unwrap();
                    };
                    self.topic.waiters.fetch_sub(1, Ordering::Release);
                    return result;
                }
            }

            strategy.wait(iter);
            iter = iter.saturating_add(1);
        }
    }

    /// Approximate number of messages pending for this subscriber.
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
