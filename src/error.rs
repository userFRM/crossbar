// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for crossbar-ipc.

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
