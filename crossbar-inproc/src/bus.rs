// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Central bus: `Bus<T>` for topic creation, publishing, and subscribing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::ring::Ring;
use crate::subscription::Subscription;
use crate::topic::{TopicHandle, TopicInner};

/// Configuration for a [`Bus`].
pub struct BusConfig {
    /// Default ring depth per subscriber. Default: 64.
    pub ring_depth: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self { ring_depth: 64 }
    }
}

/// A type-safe, in-process pub/sub message bus.
///
/// `Bus<T>` manages topics and subscriptions for messages of type `T`.
/// All messages are shared via `Arc<T>` for zero-copy fan-out.
///
/// The bus is `Clone` (cheap `Arc` clone) and can be shared across threads.
///
/// # Architecture
///
/// Each subscriber gets a dedicated lock-free SPSC ring. Publishing iterates
/// all subscriber rings and pushes `Arc::clone` into each — O(N) in subscriber
/// count, but each push is ~19ns.
///
/// # Example
///
/// ```
/// use crossbar_inproc::prelude::*;
/// use std::sync::Arc;
///
/// let bus = Bus::<String>::new();
/// let topic = bus.topic("greetings");
/// let sub = bus.subscribe("greetings");
///
/// topic.publish(Arc::new("hello".into()));
/// assert_eq!(*sub.try_recv().unwrap(), "hello");
/// ```
pub struct Bus<T> {
    inner: Arc<BusInner<T>>,
}

impl<T> Clone for Bus<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct BusInner<T> {
    topics: Mutex<HashMap<String, Arc<TopicInner<T>>>>,
    config: BusConfig,
}

impl<T: Send + Sync + 'static> Bus<T> {
    /// Create a new bus with default configuration.
    pub fn new() -> Self {
        Self::with_config(BusConfig::default())
    }

    /// Create a new bus with custom configuration.
    pub fn with_config(config: BusConfig) -> Self {
        Self {
            inner: Arc::new(BusInner {
                topics: Mutex::new(HashMap::new()),
                config,
            }),
        }
    }

    /// Get a pre-resolved handle for a topic, creating it if needed.
    ///
    /// The returned [`TopicHandle`] avoids hash lookups on the publish
    /// hot path — use this for high-frequency publishing.
    pub fn topic(&self, name: &str) -> TopicHandle<T> {
        let mut topics = self.inner.topics.lock().unwrap();
        let inner = topics
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(TopicInner::new()))
            .clone();

        TopicHandle {
            inner,
            name: Arc::from(name),
        }
    }

    /// Publish a message to a topic by name (convenience method).
    ///
    /// This performs a hash lookup on every call. For high-frequency
    /// publishing, use [`Bus::topic()`] to get a [`TopicHandle`] instead.
    pub fn publish(&self, topic: &str, msg: Arc<T>) {
        let topics = self.inner.topics.lock().unwrap();
        if let Some(inner) = topics.get(topic) {
            let handle = TopicHandle {
                inner: Arc::clone(inner),
                name: Arc::from(topic),
            };
            drop(topics); // release lock before publish
            handle.publish(msg);
        }
    }

    /// Subscribe to a topic with the default ring depth.
    ///
    /// Creates the topic if it doesn't exist. The returned
    /// [`Subscription`] automatically unsubscribes on drop.
    ///
    /// New subscribers start with an empty ring — they will not see
    /// messages published before they subscribed.
    pub fn subscribe(&self, topic: &str) -> Subscription<T> {
        self.subscribe_with_depth(topic, self.inner.config.ring_depth)
    }

    /// Subscribe to a topic with a custom ring depth.
    ///
    /// Each subscriber gets its own dedicated SPSC ring of the given depth.
    pub fn subscribe_with_depth(&self, topic: &str, depth: usize) -> Subscription<T> {
        let mut topics = self.inner.topics.lock().unwrap();
        let inner = topics
            .entry(topic.to_string())
            .or_insert_with(|| Arc::new(TopicInner::new()))
            .clone();
        drop(topics); // release lock before subscriber ops

        let ring = Arc::new(Ring::new(depth));
        let id = inner.add_subscriber(Arc::clone(&ring));
        Subscription::new(ring, inner, id)
    }

    /// List all topic names.
    pub fn topics(&self) -> Vec<String> {
        self.inner.topics.lock().unwrap().keys().cloned().collect()
    }

    /// Number of topics currently in the bus.
    pub fn topic_count(&self) -> usize {
        self.inner.topics.lock().unwrap().len()
    }
}

impl<T: Send + Sync + 'static> Default for Bus<T> {
    fn default() -> Self {
        Self::new()
    }
}
