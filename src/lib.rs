// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! # crossbar
//!
//! **Zero-copy pub/sub over shared memory. URI-addressed. O(1) transfer at any payload size.**
//!
//! Transfers an 8-byte descriptor through a lock-free ring — O(1) regardless
//! of payload. Subscribers read directly from shared memory via [`Sample`]
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
//! let mut pub_ = Publisher::create("prices", Config::default()).unwrap();
//! let topic = pub_.register("/tick/AAPL").unwrap();
//!
//! let mut loan = pub_.loan(&topic).unwrap();
//! loan.set_data(b"hello").unwrap();
//! loan.publish(); // O(1) — writes 8 bytes to ring
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod pod;
#[allow(unsafe_code)]
pub mod protocol;
#[allow(unsafe_code)]
pub mod wait;

pub mod error;

#[allow(unsafe_code)]
mod platform;

#[allow(unsafe_code)]
#[cfg(feature = "ffi")]
pub mod ffi;

pub use pod::Pod;
pub use protocol::{Config, Region};
pub use wait::WaitStrategy;

pub use platform::{
    BusSubscriber, Channel, DiscoveredTopic, Loan, PinnedGuard, PinnedLoan, PodBus, Publisher,
    Registry, Sample, Stream, Subscriber, Topic, TypedLoan, TypedSample,
};

/// Discover topics matching a URI pattern.
///
/// Opens the global registry, prunes stale entries (older than 10 seconds),
/// and returns all topics whose URI matches `pattern`.
///
/// Pattern supports trailing `*` wildcard: `"/tick/*"` matches `"/tick/AAPL"`.
///
/// # Errors
///
/// Returns an error if the registry file cannot be opened or created.
pub fn discover(pattern: &str) -> Result<Vec<DiscoveredTopic>, error::Error> {
    let reg = Registry::open()?;
    reg.prune_stale(std::time::Duration::from_secs(10));
    Ok(reg.discover(pattern))
}
