// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Error types for shared-memory pub/sub operations.

use core::fmt;

/// Error type for shared-memory pub/sub operations.
#[derive(Debug)]
pub enum IpcError {
    /// An I/O error occurred on the underlying file or mmap.
    #[cfg(feature = "std")]
    Io(std::io::Error),

    /// The shared-memory publisher has stopped updating its heartbeat.
    PublisherDead,

    /// The shared-memory region has invalid magic, version, or metadata.
    InvalidRegion(alloc::string::String),

    /// The block pool is exhausted (all blocks are in use by subscribers).
    PoolExhausted,

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
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "std")]
            IpcError::Io(e) => write!(f, "I/O error: {e}"),
            IpcError::PublisherDead => {
                write!(f, "shared-memory publisher is dead (heartbeat stale)")
            }
            IpcError::InvalidRegion(msg) => {
                write!(f, "invalid shared-memory region: {msg}")
            }
            IpcError::PoolExhausted => {
                write!(
                    f,
                    "block pool exhausted -- increase block_count in PubSubConfig"
                )
            }
            IpcError::DataTooLarge { size, capacity } => {
                write!(f, "data ({size}) exceeds block data capacity ({capacity})")
            }
            IpcError::PinnedReadersActive { count, topic_idx } => {
                write!(
                    f,
                    "{count} active PinnedGuard(s) on topic {topic_idx} -- \
                     drop all guards before calling loan_pinned"
                )
            }
            IpcError::ClockError => {
                write!(f, "system clock is before UNIX epoch")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IpcError::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for IpcError {
    #[inline]
    fn from(e: std::io::Error) -> Self {
        IpcError::Io(e)
    }
}
