// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for pool-backed O(1) pub/sub.

use core::time::Duration;

/// Configuration for pool-backed O(1) pub/sub.
#[derive(Debug, Clone, Copy)]
pub struct PubSubConfig {
    /// Maximum number of topics (default: 16).
    pub max_topics: u32,
    /// Number of blocks in the shared pool (default: 256).
    pub block_count: u32,
    /// Size of each block in bytes (default: 65536 = 64 KiB).
    /// Usable data capacity is `block_size - 8` (8 bytes for refcount header).
    pub block_size: u32,
    /// Ring depth per topic -- how many published samples the ring remembers
    /// before overwriting (default: 8). Must be a power of 2.
    pub ring_depth: u32,
    /// Heartbeat write interval (default: 100 ms).
    pub heartbeat_interval: Duration,
    /// Publisher considered dead after this duration without heartbeat (default: 5 s).
    pub stale_timeout: Duration,
}

impl Default for PubSubConfig {
    fn default() -> Self {
        Self {
            max_topics: 16,
            block_count: 256,
            block_size: 65536,
            ring_depth: 8,
            heartbeat_interval: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(5),
        }
    }
}
