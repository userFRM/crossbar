// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! # crossbar
//!
//! **Zero-copy pub/sub over shared memory. URI-addressed. O(1) transfer at any payload size.**
//!
//! Transfers an 8-byte descriptor through a lock-free ring — O(1) regardless
//! of payload. Subscribers read directly from shared memory via [`SampleGuard`]
//! — no copy, no serialization, no service discovery layer.
//!
//! Supported platforms: Linux, macOS, Windows.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use crossbar::*;
//!
//! // Publisher
//! let mut pub_ = ShmPublisher::create("prices", PubSubConfig::default()).unwrap();
//! let topic = pub_.register("/tick/AAPL").unwrap();
//!
//! let mut loan = pub_.loan(&topic);
//! loan.set_data(b"hello");
//! loan.publish(); // O(1) — writes 8 bytes to ring
//! ```

#![no_std]
#![warn(missing_docs)]
#![deny(unsafe_code)]

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

#[allow(unsafe_code)]
mod pod;
#[cfg_attr(not(feature = "std"), allow(dead_code))]
#[allow(unsafe_code)]
pub mod protocol;
#[cfg_attr(not(feature = "std"), allow(dead_code))]
#[allow(unsafe_code)]
pub mod wait;

pub mod error;

#[cfg(feature = "std")]
#[allow(unsafe_code)]
mod platform;

#[cfg(feature = "ffi")]
#[allow(unsafe_code)]
pub mod ffi;

// Always available (no_std)
pub use pod::Pod;
pub use protocol::{PubSubConfig, Region};
pub use wait::WaitStrategy;

// std-only exports
#[cfg(feature = "std")]
pub use platform::{
    SampleGuard, ShmChannel, ShmLoan, ShmPublisher, ShmSubscriber, Subscription, TopicHandle,
    TypedSampleGuard, TypedShmLoan,
};
