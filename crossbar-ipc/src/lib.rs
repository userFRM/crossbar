// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! # crossbar-ipc
//!
//! **Zero-copy O(1) pub/sub over shared memory.**
//!
//! Allocates blocks from a lock-free pool (Treiber stack), writes data
//! directly into the mmap'd region, and transfers ownership via an 8-byte
//! descriptor. Subscribers get safe zero-copy access through [`SampleGuard`].
//!
//! Supported platforms: Linux, macOS, Windows.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use crossbar_ipc::*;
//!
//! // Publisher
//! let mut pub_ = ShmPublisher::create("prices", PubSubConfig::default()).unwrap();
//! let topic = pub_.register("/tick/AAPL").unwrap();
//!
//! let mut loan = pub_.loan(&topic);
//! loan.set_data(b"hello");
//! loan.publish();
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod mmap;
#[allow(unsafe_code)]
mod notify;
#[allow(unsafe_code)]
mod pubsub;
#[allow(unsafe_code)]
mod wait;

pub mod error;

pub use pubsub::{
    PubSubConfig, SampleGuard, ShmLoan, ShmPublisher, ShmSubscriber, Subscription, TopicHandle,
};
pub use wait::WaitStrategy;
