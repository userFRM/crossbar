// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Topic management: `TopicHandle` for publish, `TopicInner` for state.
//!
//! Each subscriber gets a dedicated SPSC ring. Publishing iterates all
//! subscriber rings and pushes `Arc::clone` into each.

use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::ring::Ring;

/// Opaque handle to a topic. Obtained from [`Bus::topic()`](crate::Bus::topic).
///
/// Holds a direct pointer to the topic's inner state, avoiding
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
    /// Iterates subscriber rings and pushes `Arc::clone` into each — O(N)
    /// in subscriber count, but each push is ~19ns.
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
    /// Subscriber rings — one per subscriber. Swapped atomically via ArcSwap.
    pub(crate) subscribers: ArcSwap<Vec<Arc<Ring<T>>>>,

    /// Subscriber IDs, parallel to `subscribers` vec.
    pub(crate) sub_ids: ArcSwap<Vec<u64>>,

    /// Mutex guarding subscribe/unsubscribe mutations.
    sub_mutex: Mutex<()>,

    /// Next subscriber ID to assign.
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
            subscribers: ArcSwap::from_pointee(Vec::new()),
            sub_ids: ArcSwap::from_pointee(Vec::new()),
            sub_mutex: Mutex::new(()),
            next_sub_id: AtomicU64::new(0),
            notify: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
            wake_condvar: Condvar::new(),
            wake_mutex: Mutex::new(()),
            publish_count: AtomicU64::new(0),
        }
    }

    /// Register a new subscriber with the given ring. Returns the subscriber ID.
    pub(crate) fn add_subscriber(&self, ring: Arc<Ring<T>>) -> u64 {
        let _lock = self.sub_mutex.lock().unwrap();
        let id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);

        let mut subs = (**self.subscribers.load()).clone();
        subs.push(ring);
        self.subscribers.store(Arc::new(subs));

        let mut ids = (**self.sub_ids.load()).clone();
        ids.push(id);
        self.sub_ids.store(Arc::new(ids));

        id
    }

    /// Remove a subscriber by ID.
    pub(crate) fn remove_subscriber(&self, id: u64) {
        let _lock = self.sub_mutex.lock().unwrap();

        let old_ids = self.sub_ids.load();
        if let Some(pos) = old_ids.iter().position(|&x| x == id) {
            let mut subs = (**self.subscribers.load()).clone();
            subs.remove(pos);
            self.subscribers.store(Arc::new(subs));

            let mut ids = (**old_ids).clone();
            ids.remove(pos);
            self.sub_ids.store(Arc::new(ids));
        }
    }
}
