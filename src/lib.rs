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
//! use crossbar::*;
//!
//! // Publisher
//! let mut pub_ = ShmPublisher::create("prices", PubSubConfig::default()).unwrap();
//! let topic = pub_.register("/tick/AAPL").unwrap();
//!
//! let mut loan = pub_.loan(&topic);
//! loan.set_data(b"hello");
//! loan.publish();
//! ```

#![no_std]
#![warn(missing_docs)]
#![deny(unsafe_code)]

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

#[allow(unsafe_code)]
mod pod;
#[allow(unsafe_code)]
pub mod protocol;
#[allow(unsafe_code)]
pub mod wait;

pub mod error;

#[cfg(feature = "std")]
#[allow(unsafe_code)]
mod platform;

// Always available (no_std)
pub use pod::Pod;
pub use protocol::{PubSubConfig, Region};
pub use wait::WaitStrategy;

// std-only exports
#[cfg(feature = "std")]
pub use platform::{
    SampleGuard, ShmLoan, ShmPublisher, ShmSubscriber, Subscription, TopicHandle, TypedSampleGuard,
    TypedShmLoan,
};
