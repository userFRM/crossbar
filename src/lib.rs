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

#[cfg(feature = "ffi")]
#[allow(unsafe_code)]
pub mod ffi;

// Always available (no_std)
pub use pod::Pod;
pub use protocol::{Config, Region};
pub use wait::WaitStrategy;

// std-only exports
#[cfg(feature = "std")]
pub use platform::{
    BusSubscriber, Channel, DiscoveredTopic, Loan, PinnedLoan, PinnedSample, PodBus, Publisher,
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
#[cfg(feature = "std")]
pub fn discover(pattern: &str) -> Result<alloc::vec::Vec<DiscoveredTopic>, error::Error> {
    let reg = Registry::open()?;
    reg.prune_stale(core::time::Duration::from_secs(10));
    Ok(reg.discover(pattern))
}

// ---- Backwards compatibility — will be removed in v1.0 ----

/// Renamed to [`Config`].
#[deprecated(since = "0.7.0", note = "renamed to Config")]
pub type PubSubConfig = Config;

/// Renamed to [`error::Error`].
#[deprecated(since = "0.7.0", note = "renamed to Error")]
pub type IpcError = error::Error;

/// Renamed to [`Publisher`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Publisher")]
pub type ShmPublisher = Publisher;

/// Renamed to [`Subscriber`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Subscriber")]
pub type ShmSubscriber = Subscriber;

/// Renamed to [`Stream`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Stream")]
pub type Subscription = Stream;

/// Renamed to [`Channel`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Channel")]
pub type ShmChannel = Channel;

/// Renamed to [`Loan`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Loan")]
pub type ShmLoan<'a> = Loan<'a>;

/// Renamed to [`TypedLoan`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to TypedLoan")]
pub type TypedShmLoan<'a, T> = TypedLoan<'a, T>;

/// Renamed to [`BusSubscriber`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to BusSubscriber")]
pub type PodBusSubscriber<T> = BusSubscriber<T>;

/// Renamed to [`Sample`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Sample")]
pub type SampleGuard<'a> = Sample<'a>;

/// Renamed to [`TypedSample`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to TypedSample")]
pub type TypedSampleGuard<'a, T> = TypedSample<'a, T>;

/// Renamed to [`PinnedSample`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to PinnedSample")]
pub type PinnedGuard<'a> = PinnedSample<'a>;

/// Renamed to [`Topic`].
#[cfg(feature = "std")]
#[deprecated(since = "0.7.0", note = "renamed to Topic")]
pub type TopicHandle = Topic;
