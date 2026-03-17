// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Layout constants and offset helpers for the shared-memory region.
//!
//! All functions are pure computation -- no OS calls.

use super::config::PubSubConfig;

pub(crate) const MAGIC: &[u8; 8] = b"XBAR_ZC\0";
pub(crate) const VERSION: u32 = 3;

pub(crate) const HEADER_SIZE: usize = 128;
pub(crate) const TOPIC_ENTRY_SIZE: usize = 128;
pub(crate) const RING_ENTRY_SIZE: usize = 16;
/// Data starts at offset 8 within each block (after free-list link + refcount).
pub(crate) const BLOCK_DATA_OFFSET: usize = 8;
pub(crate) const NO_BLOCK: u32 = u32::MAX;

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
pub(crate) const TE_NOTIFY: usize = 4; // AtomicU32
pub(crate) const TE_WRITE_SEQ: usize = 8; // AtomicU64
pub(crate) const TE_WAITERS: usize = 0x10; // AtomicU32 -- moved from 0x5C to 1st cache line
pub(crate) const TE_URI_LEN: usize = 0x14; // u32
pub(crate) const TE_URI_HASH: usize = 0x18; // u64
pub(crate) const TE_URI: usize = 0x20; // char[64]
pub(crate) const TE_URI_MAX: usize = 64; // unchanged (0x60 - 0x20 = 64)
pub(crate) const TE_TYPE_SIZE: usize = 0x60; // u32 -- size_of::<T>() for typed topics, 0 = untyped

// Ring entry offsets (relative to entry start)
pub(crate) const RE_SEQ: usize = 0; // AtomicU64 (seqlock)
pub(crate) const RE_BLOCK_IDX: usize = 8; // u32
pub(crate) const RE_DATA_LEN: usize = 12; // u32

// Block offsets (relative to block start)
// 0x00: next_free_idx (u32) -- only valid when block is free
// 0x04: refcount (AtomicU32) -- always valid
// 0x08: data start

pub(crate) const BK_REFCOUNT: usize = 4;

#[inline]
pub(crate) fn topic_entry_off(idx: u32) -> usize {
    HEADER_SIZE + idx as usize * TOPIC_ENTRY_SIZE
}

pub(crate) fn ring_base(config: &PubSubConfig) -> usize {
    HEADER_SIZE + config.max_topics as usize * TOPIC_ENTRY_SIZE
}

#[inline]
pub(crate) fn ring_entry_off(config: &PubSubConfig, topic_idx: u32, slot: u32) -> usize {
    ring_base(config)
        + topic_idx as usize * config.ring_depth as usize * RING_ENTRY_SIZE
        + slot as usize * RING_ENTRY_SIZE
}

pub(crate) fn block_pool_offset(config: &PubSubConfig) -> usize {
    ring_base(config) + config.max_topics as usize * config.ring_depth as usize * RING_ENTRY_SIZE
}

pub(crate) fn region_size(config: &PubSubConfig) -> usize {
    block_pool_offset(config) + config.block_count as usize * config.block_size as usize
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
