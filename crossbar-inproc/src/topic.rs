// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Topic management: `TopicHandle` for O(1) publish, `TopicInner` for state.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use arc_swap::ArcSwap;

use crate::ring::Ring;

/// Opaque handle to a topic. Obtained from [`Bus::topic()`](crate::Bus::topic).
///
/// Holds a direct pointer to the topic's subscriber list, avoiding
/// any hash lookup on the publish hot path.
///
/// `TopicHandle` is `Clone`, `Send`, and `Sync`.
pub struct TopicHandle<T> {
    pub(crate) inner: Arc<TopicInner<T>>,
    pub(crate) name: Arc<str>,
}

impl<T> Clone for TopicHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            name: Arc::clone(&self.name),
        }
    }
}

impl<T: Send + Sync + 'static> TopicHandle<T> {
    /// Publish a message to all current subscribers.
    ///
    /// Each subscriber's ring receives an `Arc::clone()` of the message.
    /// If a subscriber's ring is full, the oldest message in that ring is
    /// dropped (lossy semantics).
    ///
    /// This is the hot path — no hash lookup, no lock acquisition.
    #[inline]
    pub fn publish(&self, msg: Arc<T>) {
        let subs = self.inner.subscribers.load();
        for ring in subs.iter() {
            ring.push(Arc::clone(&msg));
        }
        self.inner.publish_count.fetch_add(1, Ordering::Relaxed);

        // Smart wake: only notify if subscribers are blocked in recv().
        self.inner.notify.fetch_add(1, Ordering::Release);
        if self.inner.waiters.load(Ordering::Acquire) > 0 {
            let _guard = self.inner.wake_mutex.lock().unwrap();
            self.inner.wake_condvar.notify_all();
        }
    }

    /// Returns the current number of subscribers.
    #[inline]
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.load().len()
    }

    /// Returns the topic name.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Total messages published to this topic.
    #[inline]
    pub fn publish_count(&self) -> u64 {
        self.inner.publish_count.load(Ordering::Relaxed)
    }
}

/// Internal state for a single topic.
pub(crate) struct TopicInner<T> {
    /// Current set of subscriber rings. Swapped atomically on
    /// subscribe/unsubscribe.
    pub(crate) subscribers: ArcSwap<Vec<Arc<Ring<T>>>>,

    /// Per-subscriber IDs for removal tracking.
    pub(crate) sub_ids: ArcSwap<Vec<u64>>,

    /// Serializes subscribe/unsubscribe (cold path only).
    sub_mutex: Mutex<()>,

    /// Next subscriber ID (monotonically increasing).
    pub(crate) next_sub_id: AtomicU64,

    /// Notification counter (bumped on every publish).
    pub(crate) notify: AtomicU32,

    /// Number of subscribers currently blocked in `recv()`.
    pub(crate) waiters: AtomicU32,

    /// Condvar for blocking `recv()` subscribers.
    pub(crate) wake_condvar: Condvar,

    /// Mutex paired with the condvar.
    pub(crate) wake_mutex: Mutex<()>,

    /// Total messages published.
    pub(crate) publish_count: AtomicU64,
}

impl<T> TopicInner<T> {
    pub(crate) fn new() -> Self {
        Self {
            subscribers: ArcSwap::new(Arc::new(Vec::new())),
            sub_ids: ArcSwap::new(Arc::new(Vec::new())),
            sub_mutex: Mutex::new(()),
            next_sub_id: AtomicU64::new(0),
            notify: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
            wake_condvar: Condvar::new(),
            wake_mutex: Mutex::new(()),
            publish_count: AtomicU64::new(0),
        }
    }

    /// Add a subscriber with a dedicated ring. Returns (id, ring).
    pub(crate) fn add_subscriber(&self, ring_depth: usize) -> (u64, Arc<Ring<T>>) {
        let _guard = self.sub_mutex.lock().unwrap();

        let id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        let ring = Arc::new(Ring::new(ring_depth));

        let old_subs = self.subscribers.load();
        let old_ids = self.sub_ids.load();

        let mut new_subs = (**old_subs).clone();
        let mut new_ids = (**old_ids).clone();

        new_subs.push(Arc::clone(&ring));
        new_ids.push(id);

        self.subscribers.store(Arc::new(new_subs));
        self.sub_ids.store(Arc::new(new_ids));

        (id, ring)
    }

    /// Remove a subscriber by ID.
    pub(crate) fn remove_subscriber(&self, id: u64) {
        let _guard = self.sub_mutex.lock().unwrap();

        let old_ids = self.sub_ids.load();
        let old_subs = self.subscribers.load();

        if let Some(pos) = old_ids.iter().position(|&x| x == id) {
            let mut new_subs = (**old_subs).clone();
            let mut new_ids = (**old_ids).clone();

            new_subs.remove(pos);
            new_ids.remove(pos);

            self.subscribers.store(Arc::new(new_subs));
            self.sub_ids.store(Arc::new(new_ids));
        }
    }
}
