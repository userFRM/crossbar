// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Layout constants and offset helpers for the shared-memory region.
//!
//! All functions are pure computation -- no OS calls.

// Crossbar uses futex on the low-32 of AtomicU64, which only works on
// little-endian. All supported platforms (x86-64, aarch64 in default mode)
// are little-endian.
#[cfg(not(target_endian = "little"))]
compile_error!("crossbar requires a little-endian target");

use super::config::Config;

pub(crate) const MAGIC: &[u8; 8] = b"XBAR_ZC\0";
pub(crate) const VERSION: u32 = 3;

pub(crate) const HEADER_SIZE: usize = 128;
pub(crate) const TOPIC_ENTRY_SIZE: usize = 128;
pub(crate) const RING_ENTRY_SIZE: usize = 16;
/// Data starts at offset 8 within each block (after free-list link + refcount).
pub(crate) const BLOCK_DATA_OFFSET: usize = 8;
pub(crate) const NO_BLOCK: u32 = u32::MAX;

/// Sentinel for seqlock "being written" state (CAS-based slot locking).
pub(crate) const SEQ_WRITING: u64 = u64::MAX;

/// Topic active states for CAS-based registration.
pub(crate) const TE_STATE_FREE: u32 = 0;
pub(crate) const TE_STATE_INIT: u32 = 2;
pub(crate) const TE_STATE_ACTIVE: u32 = 1;

/// Maximum allowed `max_topics` to prevent OOM from malicious headers.
pub(crate) const MAX_TOPICS_LIMIT: u32 = 4096;

/// Writer-active sentinel for pinned publish. Stored in TE_PINNED_READERS
/// via CAS to atomically prevent subscribers from entering while the
/// publisher is writing. Must not collide with valid reader counts.
pub(crate) const PINNED_WRITER_ACTIVE: u32 = u32::MAX;

// Global header offsets
pub(crate) const GH_MAGIC: usize = 0;
pub(crate) const GH_VERSION: usize = 8;
pub(crate) const GH_MAX_TOPICS: usize = 0x0C;
pub(crate) const GH_BLOCK_COUNT: usize = 0x10;
pub(crate) const GH_BLOCK_SIZE: usize = 0x14;
pub(crate) const GH_RING_DEPTH: usize = 0x18;
pub(crate) const GH_POOL_HEAD: usize = 0x20; // AtomicU64 (Treiber stack)
pub(crate) const GH_HEARTBEAT: usize = 0x28; // AtomicU64
pub(crate) const GH_PID: usize = 0x30;
pub(crate) const GH_STALE_TIMEOUT_US: usize = 0x38;

// Topic entry offsets (relative to entry start)
pub(crate) const TE_ACTIVE: usize = 0; // AtomicU32
pub(crate) const TE_WRITE_SEQ: usize = 8; // AtomicU64
pub(crate) const TE_WAITERS: usize = 0x10; // AtomicU32
pub(crate) const TE_URI_LEN: usize = 0x14; // u32
pub(crate) const TE_URI_HASH: usize = 0x18; // u64
pub(crate) const TE_URI: usize = 0x20; // char[64]
pub(crate) const TE_URI_MAX: usize = 64;
pub(crate) const TE_TYPE_SIZE: usize = 0x60; // u32

/// Pinned-publish block index (u32). NO_BLOCK = not pinned.
pub(crate) const TE_PINNED_BLOCK: usize = 0x64;
/// Pinned-publish active reader count (AtomicU32).
pub(crate) const TE_PINNED_READERS: usize = 0x68;
/// Pinned-publish seqlock: packed (seq:32 | data_len:32) in AtomicU64.
pub(crate) const TE_PINNED_SEQ: usize = 0x70;
/// Per-topic subscriber count (AtomicU32). Incremented on subscribe, decremented on drop.
pub(crate) const TE_SUBSCRIBER_COUNT: usize = 0x78;

// Ring entry offsets (relative to entry start)
pub(crate) const RE_SEQ: usize = 0; // AtomicU64 (seqlock)
pub(crate) const RE_BLOCK_IDX: usize = 8; // u32
pub(crate) const RE_DATA_LEN: usize = 12; // u32

// Block offsets
pub(crate) const BK_REFCOUNT: usize = 4;

#[inline]
pub(crate) fn topic_entry_off(idx: u32) -> usize {
    HEADER_SIZE + idx as usize * TOPIC_ENTRY_SIZE
}

pub(crate) fn ring_base(config: &Config) -> usize {
    HEADER_SIZE + config.max_topics as usize * TOPIC_ENTRY_SIZE
}

#[inline]
pub(crate) fn ring_entry_off(config: &Config, topic_idx: u32, slot: u32) -> usize {
    ring_base(config)
        + topic_idx as usize * config.ring_depth as usize * RING_ENTRY_SIZE
        + slot as usize * RING_ENTRY_SIZE
}

/// Compute block pool offset with checked arithmetic. Returns `None` on overflow.
pub(crate) fn block_pool_offset_checked(config: &Config) -> Option<usize> {
    let rb = ring_base(config);
    let ring_size = (config.max_topics as usize)
        .checked_mul(config.ring_depth as usize)?
        .checked_mul(RING_ENTRY_SIZE)?;
    rb.checked_add(ring_size)
}

pub(crate) fn block_pool_offset(config: &Config) -> usize {
    // Only called after config is validated and region_size_checked passed.
    ring_base(config) + config.max_topics as usize * config.ring_depth as usize * RING_ENTRY_SIZE
}

/// Compute the total region size with fully checked arithmetic. Returns `None` on overflow.
pub(crate) fn region_size_checked(config: &Config) -> Option<usize> {
    let pool_off = block_pool_offset_checked(config)?;
    let pool_size = (config.block_count as usize).checked_mul(config.block_size as usize)?;
    pool_off.checked_add(pool_size)
}

pub(crate) fn uri_hash(uri: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for b in uri.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3); // FNV prime
    }
    h
}

// ---- Treiber stack helpers ----

#[inline]
pub(crate) fn pack(gen: u32, idx: u32) -> u64 {
    u64::from(gen) << 32 | u64::from(idx)
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn unpack(val: u64) -> (u32, u32) {
    ((val >> 32) as u32, val as u32)
}

/// Validate that a segment name contains no path traversal characters.
pub(crate) fn is_valid_segment_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    !name.contains('/') && !name.contains('\\') && !name.contains('\0') && !name.contains("..")
}
