// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Error types for shared-memory pub/sub operations.

use std::fmt;

/// Error type for shared-memory pub/sub operations.
#[derive(Debug)]
pub enum Error {
    /// An I/O error occurred on the underlying file or mmap.
    Io(std::io::Error),

    /// The shared-memory publisher has stopped updating its heartbeat.
    PublisherDead,

    /// The shared-memory region has invalid magic, version, or metadata.
    InvalidRegion(String),

    /// The block pool is exhausted (all blocks are in use by subscribers).
    PoolExhausted,

    /// The bounded PodBus ring is full (backpressure).
    ///
    /// Returned by [`crate::PodBus::try_publish`] when the slowest subscriber is
    /// too far behind the publisher. The caller should wait or drop the value.
    Full,

    /// Data exceeds the block's data capacity.
    DataTooLarge {
        /// Attempted write size.
        size: usize,
        /// Block data capacity.
        capacity: usize,
    },

    /// A subscriber holds a `PinnedGuard`, preventing the publisher from writing.
    PinnedReadersActive {
        /// Number of active readers.
        count: u32,
        /// Topic index.
        topic_idx: u32,
    },

    /// The system clock is before UNIX epoch (NTP jump, VM restore).
    ClockError,

    /// A subscribe() call could not find the requested URI in the topic table.
    TopicNotFound(String),

    /// A register() call ran out of topic slots.
    MaxTopicsReached,

    /// The topic URI exceeds the maximum allowed length.
    UriTooLong {
        /// Actual URI length.
        len: usize,
        /// Maximum allowed length.
        max: usize,
    },

    /// An exclusive lock is already held on the region (another publisher is active).
    LockContention(String),

    /// A `Pod` type's alignment exceeds the block data alignment.
    AlignmentError {
        /// Actual type alignment.
        align: usize,
        /// Maximum supported alignment.
        max: usize,
    },

    /// The segment name is invalid (empty, contains path separators, null bytes, or `..`).
    SegmentNameInvalid(String),

    /// A `Topic` was used with a different `Publisher` than the one that created it.
    HandleMismatch,

    /// Checked arithmetic overflow during region size computation.
    RegionSizeOverflow,

    /// The shared-memory region's free-list is corrupted (out-of-bounds block index).
    RegionCorrupted(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::PublisherDead => {
                write!(f, "shared-memory publisher is dead (heartbeat stale)")
            }
            Error::InvalidRegion(msg) => {
                write!(f, "invalid shared-memory region: {msg}")
            }
            Error::PoolExhausted => {
                write!(f, "block pool exhausted -- increase block_count in Config")
            }
            Error::Full => {
                write!(
                    f,
                    "bounded PodBus ring is full -- slowest subscriber is too far behind"
                )
            }
            Error::DataTooLarge { size, capacity } => {
                write!(f, "data ({size}) exceeds block data capacity ({capacity})")
            }
            Error::PinnedReadersActive { count, topic_idx } => {
                write!(
                    f,
                    "{count} active PinnedGuard(s) on topic {topic_idx} -- \
                     drop all guards before calling loan_pinned"
                )
            }
            Error::ClockError => {
                write!(f, "system clock is before UNIX epoch")
            }
            Error::TopicNotFound(uri) => {
                write!(f, "topic '{uri}' not found")
            }
            Error::MaxTopicsReached => {
                write!(f, "maximum topics reached")
            }
            Error::UriTooLong { len, max } => {
                write!(f, "topic URI too long ({len} > {max})")
            }
            Error::LockContention(name) => {
                write!(f, "pub/sub region '{name}' is already active (lock held)")
            }
            Error::AlignmentError { align, max } => {
                write!(
                    f,
                    "Pod type alignment ({align}) exceeds block data offset ({max})"
                )
            }
            Error::SegmentNameInvalid(name) => {
                write!(
                    f,
                    "invalid segment name '{name}': must be non-empty and contain \
                     no '/', '\\', '..', or null bytes"
                )
            }
            Error::HandleMismatch => {
                write!(f, "Topic belongs to a different Publisher")
            }
            Error::RegionSizeOverflow => {
                write!(f, "region size computation overflow")
            }
            Error::RegionCorrupted(msg) => {
                write!(f, "shared-memory region corrupted: {msg}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    #[inline]
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
