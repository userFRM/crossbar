// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ultra-fast in-process pub/sub bus with type-safe topics.
//!
//! `crossbar-inproc` provides a generic [`Bus<T>`] for zero-copy message
//! fan-out within a single process. Messages are shared via `Arc<T>`,
//! avoiding serialization entirely.
//!
//! # Quick Start
//!
//! ```
//! use crossbar_inproc::prelude::*;
//! use std::sync::Arc;
//!
//! let bus = Bus::<u64>::new();
//! let topic = bus.topic("prices");
//! let sub = bus.subscribe("prices");
//!
//! topic.publish(Arc::new(42));
//! assert_eq!(*sub.try_recv().unwrap(), 42);
//! ```
//!
//! # Architecture
//!
//! - **[`Bus<T>`]** — central registry of topics and subscriptions (cold path)
//! - **[`TopicHandle<T>`]** — pre-resolved handle for O(N) publish (hot path)
//! - **[`Subscription<T>`]** — per-subscriber SPSC ring consumer
//!
//! Each subscriber gets a dedicated lock-free SPSC ring. Publishing iterates
//! all subscriber rings and pushes `Arc::clone` into each — O(N) in subscriber
//! count, but each push is ~19ns.

mod bus;
#[allow(unsafe_code)]
pub(crate) mod ring;
mod subscription;
mod topic;
#[allow(unsafe_code)]
pub mod wait;

pub use bus::{Bus, BusConfig};
pub use subscription::Subscription;
pub use topic::TopicHandle;
pub use wait::WaitStrategy;

/// Convenience re-exports for `use crossbar_inproc::prelude::*`.
pub mod prelude {
    pub use crate::{Bus, BusConfig, Subscription, TopicHandle, WaitStrategy};
}
