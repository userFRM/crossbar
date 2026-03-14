// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(unsafe_code)]
#![allow(dead_code)] // Types are used by consumers; not internally cross-referenced

//! Pool-backed O(1) zero-copy pub/sub over shared memory.
//!
//! Unlike ring-based pub/sub (where data is copied into ring slots), pool-backed
//! pub/sub allocates independent buffers from a shared pool and transfers only
//! an 8-byte descriptor (block index + data length) regardless of payload size.
//!
//! The subscriber gets a **safe** zero-copy reference (`ShmPoolSampleGuard`) that
//! holds the block alive via atomic refcounting. No `unsafe` needed to read data —
//! unlike ring-based `ShmSampleRef` where the publisher can overwrite concurrently.
//!
//! Protocol: seqlock (same as ring pub/sub) + atomic refcount per block.
//! Publisher sets refcount=1 on publish. Subscriber CAS-increments before reading.
//! On ring overwrite, publisher decrements old block's refcount. Guard decrements
//! on drop. Block freed when refcount hits 0.

use std::io;
use std::ops::Deref;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::mmap::RawMmap;
use super::notify;
use crate::error::CrossbarError;

// ─── Layout constants ────────────────────────────────────────────────────

const MAGIC: &[u8; 8] = b"XBAR_ZC\0";
const VERSION: u32 = 1;

const HEADER_SIZE: usize = 128;
const TOPIC_ENTRY_SIZE: usize = 128;
const RING_ENTRY_SIZE: usize = 16;
/// Data starts at offset 8 within each block (after free-list link + refcount).
const BLOCK_DATA_OFFSET: usize = 8;
const NO_BLOCK: u32 = u32::MAX;

// Global header offsets
const GH_MAGIC: usize = 0;
const GH_VERSION: usize = 8;
const GH_MAX_TOPICS: usize = 0x0C;
const GH_BLOCK_COUNT: usize = 0x10;
const GH_BLOCK_SIZE: usize = 0x14;
const GH_RING_DEPTH: usize = 0x18;
const GH_POOL_HEAD: usize = 0x20; // AtomicU64 (Treiber stack)
const GH_HEARTBEAT: usize = 0x28; // AtomicU64
const GH_PID: usize = 0x30;
const GH_STALE_TIMEOUT_US: usize = 0x38;

// Topic entry offsets (relative to entry start)
const TE_ACTIVE: usize = 0; // AtomicU32
const TE_NOTIFY: usize = 4; // AtomicU32
const TE_WRITE_SEQ: usize = 8; // AtomicU64
const TE_URI_HASH: usize = 0x10;
const TE_URI_LEN: usize = 0x18;
const TE_URI: usize = 0x1C;
const TE_URI_MAX: usize = 64;
const TE_WAITERS: usize = 0x5C; // AtomicU32, tracks # of subscribers in futex_wait

// Ring entry offsets (relative to entry start)
const RE_SEQ: usize = 0; // AtomicU64 (seqlock)
const RE_BLOCK_IDX: usize = 8; // u32
const RE_DATA_LEN: usize = 12; // u32

// Block offsets (relative to block start)
// 0x00: next_free_idx (u32) — only valid when block is free
// 0x04: refcount (AtomicU32) — always valid
// 0x08: data start

const BK_REFCOUNT: usize = 4;

// ─── Config ──────────────────────────────────────────────────────────────

/// Configuration for pool-backed O(1) pub/sub.
#[derive(Debug, Clone)]
pub struct PoolPubSubConfig {
    /// Maximum number of topics (default: 16).
    pub max_topics: u32,
    /// Number of blocks in the shared pool (default: 256).
    pub block_count: u32,
    /// Size of each block in bytes (default: 65536 = 64 KiB).
    /// Usable data capacity is `block_size - 8` (8 bytes for refcount header).
    pub block_size: u32,
    /// Ring depth per topic — how many published samples the ring remembers
    /// before overwriting (default: 8).
    pub ring_depth: u32,
    /// Heartbeat write interval (default: 100 ms).
    pub heartbeat_interval: Duration,
    /// Publisher considered dead after this duration without heartbeat (default: 5 s).
    pub stale_timeout: Duration,
}

impl Default for PoolPubSubConfig {
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

// ─── Layout helpers ──────────────────────────────────────────────────────

fn topic_entry_off(idx: u32) -> usize {
    HEADER_SIZE + idx as usize * TOPIC_ENTRY_SIZE
}

fn ring_base(config: &PoolPubSubConfig) -> usize {
    HEADER_SIZE + config.max_topics as usize * TOPIC_ENTRY_SIZE
}

fn ring_entry_off(config: &PoolPubSubConfig, topic_idx: u32, slot: u32) -> usize {
    ring_base(config)
        + topic_idx as usize * config.ring_depth as usize * RING_ENTRY_SIZE
        + slot as usize * RING_ENTRY_SIZE
}

fn block_pool_offset(config: &PoolPubSubConfig) -> usize {
    ring_base(config) + config.max_topics as usize * config.ring_depth as usize * RING_ENTRY_SIZE
}

fn region_size(config: &PoolPubSubConfig) -> usize {
    block_pool_offset(config) + config.block_count as usize * config.block_size as usize
}

fn uri_hash(uri: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for b in uri.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3); // FNV prime
    }
    h
}

fn shm_path(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/dev/shm/crossbar-pool-{name}"))
    } else {
        PathBuf::from(format!("/tmp/crossbar-pool-shm-{name}"))
    }
}

fn lock_path(name: &str) -> PathBuf {
    let mut p = shm_path(name);
    p.set_extension("lock");
    p
}

// ─── Treiber stack helpers ───────────────────────────────────────────────

#[inline]
fn pack(gen: u32, idx: u32) -> u64 {
    u64::from(gen) << 32 | u64::from(idx)
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn unpack(val: u64) -> (u32, u32) {
    ((val >> 32) as u32, val as u32)
}

// ─── PoolRegion (shared between publisher and subscriber) ────────────────

/// Shared state for the mmap region — held by both publisher/subscriber
/// and by `ShmPoolSampleGuard` (via `Arc`) to keep the mmap alive.
struct PoolRegion {
    mmap: RawMmap,
    config: PoolPubSubConfig,
    pool_offset: usize,
}

impl PoolRegion {
    #[inline]
    fn pool_head(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(GH_POOL_HEAD) as *const AtomicU64) }
    }

    #[inline]
    fn block_ptr(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        unsafe {
            self.mmap
                .as_ptr()
                .add(self.pool_offset + idx as usize * self.config.block_size as usize)
                .cast_mut()
        }
    }

    #[inline]
    fn block_refcount(&self, idx: u32) -> &AtomicU32 {
        unsafe { &*(self.block_ptr(idx).add(BK_REFCOUNT) as *const AtomicU32) }
    }

    #[inline]
    fn alloc_block(&self) -> Option<u32> {
        loop {
            let head = self.pool_head().load(Ordering::Acquire);
            let (gen, idx) = unpack(head);
            if idx == NO_BLOCK {
                return None;
            }
            let next = unsafe {
                let mut buf = [0u8; 4];
                std::ptr::copy_nonoverlapping(self.block_ptr(idx), buf.as_mut_ptr(), 4);
                u32::from_le_bytes(buf)
            };
            let new_head = pack(gen.wrapping_add(1), next);
            if self
                .pool_head()
                .compare_exchange(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Clear refcount for the newly allocated block
                self.block_refcount(idx).store(0, Ordering::Release);
                return Some(idx);
            }
        }
    }

    #[inline]
    fn free_block(&self, idx: u32) {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        loop {
            let head = self.pool_head().load(Ordering::Acquire);
            let (gen, old_head_idx) = unpack(head);
            // Write next pointer into the block's free-list link (offset 0)
            unsafe {
                std::ptr::copy_nonoverlapping(
                    old_head_idx.to_le_bytes().as_ptr(),
                    self.block_ptr(idx),
                    4,
                );
            }
            let new_head = pack(gen.wrapping_add(1), idx);
            if self
                .pool_head()
                .compare_exchange(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn init_free_list(&self) {
        for i in 0..self.config.block_count {
            let next = if i + 1 < self.config.block_count {
                i + 1
            } else {
                NO_BLOCK
            };
            unsafe {
                let ptr = self.block_ptr(i);
                std::ptr::copy_nonoverlapping(next.to_le_bytes().as_ptr(), ptr, 4);
                // Zero refcount
                (ptr.add(BK_REFCOUNT) as *mut u32).write(0);
            }
        }
        self.pool_head().store(pack(0, 0), Ordering::Release);
    }

    /// Data capacity per block (block_size minus the 8-byte header).
    fn data_capacity(&self) -> usize {
        self.config.block_size as usize - BLOCK_DATA_OFFSET
    }

    fn heartbeat_atom(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(GH_HEARTBEAT) as *const AtomicU64) }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn update_heartbeat(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        self.heartbeat_atom().store(now, Ordering::Release);
    }

    #[allow(clippy::cast_possible_truncation)]
    fn check_heartbeat(&self) -> Result<(), CrossbarError> {
        let hb = self.heartbeat_atom().load(Ordering::Acquire);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        if now.saturating_sub(hb) > self.config.stale_timeout.as_micros() as u64 {
            return Err(CrossbarError::ShmServerDead);
        }
        Ok(())
    }
}

// ─── ShmPoolPublisher ────────────────────────────────────────────────────

/// O(1) zero-copy publisher over shared memory.
///
/// Uses a shared block pool (Treiber stack) for data storage and a ring of
/// block indices for publication. Transfer cost is O(1) regardless of payload
/// size — only 8 bytes (block index + data length) are written to the ring.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// let mut pub_ = ShmPoolPublisher::create("prices", PoolPubSubConfig::default()).unwrap();
/// let topic = pub_.register("/tick/AAPL").unwrap();
///
/// let mut loan = pub_.loan(&topic);
/// loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
/// loan.set_len(8);
/// loan.publish(); // O(1) — writes 8 bytes to ring
/// ```
pub struct ShmPoolPublisher {
    region: Arc<PoolRegion>,
    path: PathBuf,
    _lock_file: std::fs::File,
    created_ino: u64,
    id: u64,
    last_heartbeat: std::time::Instant,
    loan_count: u32,
}

/// Handle returned by [`ShmPoolPublisher::register`]. Identifies a topic
/// for use with [`ShmPoolPublisher::loan`].
#[derive(Clone)]
pub struct PoolTopicHandle {
    topic_idx: u32,
    publisher_id: u64,
}

impl ShmPoolPublisher {
    /// Creates a new pool-backed pub/sub region.
    ///
    /// # Errors
    ///
    /// Returns [`CrossbarError::Io`] if the backing file cannot be created,
    /// or [`CrossbarError::ShmInvalidRegion`] if another publisher is active.
    pub fn create(name: &str, config: PoolPubSubConfig) -> Result<Self, CrossbarError> {
        // Validate config to prevent panics from invalid values
        if config.block_size < BLOCK_DATA_OFFSET as u32 + 1 {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "block_size must be at least {} (got {})",
                BLOCK_DATA_OFFSET + 1,
                config.block_size
            )));
        }
        if config.ring_depth == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "ring_depth must be at least 1".to_string(),
            ));
        }
        if config.block_count == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "block_count must be at least 1".to_string(),
            ));
        }
        if config.max_topics == 0 {
            return Err(CrossbarError::ShmInvalidRegion(
                "max_topics must be at least 1".to_string(),
            ));
        }

        let path = shm_path(name);
        let lpath = lock_path(name);

        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lpath)
            .map_err(CrossbarError::Io)?;

        // Exclusive flock — only one publisher per region
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "pool pub/sub region '{name}' is already active (flock held)"
            )));
        }

        // Remove stale file
        let _ = std::fs::remove_file(&path);

        let size = region_size(&config);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(CrossbarError::Io)?;
        file.set_len(size as u64).map_err(CrossbarError::Io)?;

        let mmap = RawMmap::from_file_with_len(&file, size).map_err(CrossbarError::Io)?;

        // Write global header
        let ptr = mmap.as_mut_ptr();
        unsafe {
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), ptr.add(GH_MAGIC), 8);
            (ptr.add(GH_VERSION) as *mut u32).write(VERSION);
            (ptr.add(GH_MAX_TOPICS) as *mut u32).write(config.max_topics);
            (ptr.add(GH_BLOCK_COUNT) as *mut u32).write(config.block_count);
            (ptr.add(GH_BLOCK_SIZE) as *mut u32).write(config.block_size);
            (ptr.add(GH_RING_DEPTH) as *mut u32).write(config.ring_depth);
            let pid = u64::from(std::process::id());
            (ptr.add(GH_PID) as *mut u64).write(pid);
            (ptr.add(GH_STALE_TIMEOUT_US) as *mut u64)
                .write(config.stale_timeout.as_micros() as u64);
        }

        let pool_off = block_pool_offset(&config);
        let region = Arc::new(PoolRegion {
            mmap,
            config: config.clone(),
            pool_offset: pool_off,
        });

        // Initialize pool free list
        region.init_free_list();
        region.update_heartbeat();

        // Initialize all ring entries to NO_BLOCK
        for t in 0..config.max_topics {
            for s in 0..config.ring_depth {
                let off = ring_entry_off(&config, t, s);
                unsafe {
                    let base = region.mmap.as_mut_ptr().add(off);
                    (base.add(RE_SEQ) as *mut u64).write(0);
                    (base.add(RE_BLOCK_IDX) as *mut u32).write(NO_BLOCK);
                    (base.add(RE_DATA_LEN) as *mut u32).write(0);
                }
            }
        }

        let created_ino = std::fs::metadata(&path).map(|m| m.ino()).unwrap_or(0);

        #[allow(clippy::cast_possible_truncation)]
        let id = u64::from(std::process::id())
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64);

        Ok(ShmPoolPublisher {
            region,
            path,
            _lock_file: lock_file,
            created_ino,
            id,
            last_heartbeat: std::time::Instant::now(),
            loan_count: 0,
        })
    }

    /// Registers a topic URI. Returns a handle for use with [`loan`](Self::loan).
    ///
    /// # Errors
    ///
    /// Returns an error if the maximum number of topics has been reached
    /// or the URI exceeds 64 bytes.
    pub fn register(&mut self, uri: &str) -> Result<PoolTopicHandle, CrossbarError> {
        if uri.len() > TE_URI_MAX {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "topic URI too long ({} > {TE_URI_MAX})",
                uri.len()
            )));
        }
        let hash = uri_hash(uri);

        for i in 0..self.region.config.max_topics {
            let off = topic_entry_off(i);
            let active =
                unsafe { &*(self.region.mmap.as_ptr().add(off + TE_ACTIVE) as *const AtomicU32) };

            if active.load(Ordering::Acquire) == 1 {
                // Check if same URI already registered (hash + byte comparison)
                let existing_hash = unsafe {
                    (self.region.mmap.as_ptr().add(off + TE_URI_HASH) as *const u64).read()
                };
                if existing_hash == hash {
                    let existing_len = unsafe {
                        (self.region.mmap.as_ptr().add(off + TE_URI_LEN) as *const u32).read()
                    } as usize;
                    let existing_bytes = unsafe {
                        std::slice::from_raw_parts(
                            self.region.mmap.as_ptr().add(off + TE_URI),
                            existing_len,
                        )
                    };
                    if existing_len == uri.len() && existing_bytes == uri.as_bytes() {
                        return Ok(PoolTopicHandle {
                            topic_idx: i,
                            publisher_id: self.id,
                        });
                    }
                    // Hash collision — different URI, keep searching
                }
                continue;
            }

            // Found free slot — register
            unsafe {
                let base = self.region.mmap.as_mut_ptr().add(off);
                (base.add(TE_URI_HASH) as *mut u64).write(hash);
                (base.add(TE_URI_LEN) as *mut u32).write(uri.len() as u32);
                std::ptr::copy_nonoverlapping(uri.as_bytes().as_ptr(), base.add(TE_URI), uri.len());
                // Initialize write_seq to 0
                (base.add(TE_WRITE_SEQ) as *mut u64).write(0);
            }
            active.store(1, Ordering::Release);

            return Ok(PoolTopicHandle {
                topic_idx: i,
                publisher_id: self.id,
            });
        }

        Err(CrossbarError::ShmInvalidRegion(
            "maximum topics reached".into(),
        ))
    }

    /// Updates the publisher heartbeat. Call this periodically during idle
    /// periods (when not calling [`loan`](Self::loan)) to prevent subscribers
    /// from treating the publisher as dead.
    ///
    /// `loan()` updates the heartbeat automatically every 1024 calls, so you
    /// only need this if the publisher may be idle longer than `stale_timeout`.
    pub fn heartbeat(&mut self) {
        self.region.update_heartbeat();
        self.last_heartbeat = std::time::Instant::now();
    }

    /// Loans a block from the pool for writing. Write your data, then call
    /// [`publish`](ShmPoolLoan::publish) to make it visible to subscribers.
    ///
    /// If the loan is dropped without publishing, the block is returned to
    /// the pool automatically.
    ///
    /// # Panics
    ///
    /// Panics if the pool is exhausted (all blocks in use) or if the handle
    /// belongs to a different publisher.
    #[inline]
    pub fn loan(&mut self, handle: &PoolTopicHandle) -> ShmPoolLoan<'_> {
        assert_eq!(
            handle.publisher_id, self.id,
            "PoolTopicHandle belongs to a different ShmPoolPublisher"
        );

        // Counter-based heartbeat: check clock every 1024 loans, not every loan.
        // Saves ~20ns per loan by avoiding Instant::now() on the hot path.
        self.loan_count = self.loan_count.wrapping_add(1);
        if self.loan_count & 0x3FF == 0
            && self.last_heartbeat.elapsed() >= self.region.config.heartbeat_interval
        {
            self.region.update_heartbeat();
            self.last_heartbeat = std::time::Instant::now();
        }

        let block_idx = self
            .region
            .alloc_block()
            .expect("pool exhausted — increase block_count in PoolPubSubConfig");

        let topic_idx = handle.topic_idx;
        let off = topic_entry_off(topic_idx);

        let write_seq_atom =
            unsafe { &*(self.region.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) };
        let next_seq = write_seq_atom.load(Ordering::Acquire) + 1;

        let notify_atom =
            unsafe { &*(self.region.mmap.as_ptr().add(off + TE_NOTIFY) as *const AtomicU32) };

        let waiters_atom =
            unsafe { &*(self.region.mmap.as_ptr().add(off + TE_WAITERS) as *const AtomicU32) };

        let data_ptr = unsafe { self.region.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };

        ShmPoolLoan {
            region: &self.region,
            data_ptr,
            capacity: self.region.data_capacity(),
            len: 0,
            block_idx,
            seq: next_seq,
            topic_idx,
            write_seq_atom,
            notify_atom,
            waiters_atom,
        }
    }
}

impl Drop for ShmPoolPublisher {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.ino() == self.created_ino {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

// ─── ShmPoolLoan ─────────────────────────────────────────────────────────

/// A mutable view into a pool block in shared memory.
///
/// Write your data (any format), then call [`publish`](Self::publish) to
/// transfer ownership to subscribers. The transfer is O(1) — only 8 bytes
/// (block index + data length) are written to the ring, regardless of how
/// much data you wrote into the block.
///
/// If dropped without publishing, the block is freed back to the pool.
pub struct ShmPoolLoan<'a> {
    region: &'a Arc<PoolRegion>,
    data_ptr: *mut u8,
    capacity: usize,
    len: usize,
    block_idx: u32,
    seq: u64,
    topic_idx: u32,
    write_seq_atom: &'a AtomicU64,
    notify_atom: &'a AtomicU32,
    waiters_atom: &'a AtomicU32,
}

impl<'a> ShmPoolLoan<'a> {
    /// Returns the writable data region as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.data_ptr, self.capacity) }
    }

    /// Copies `data` into the block starting at offset 0.
    ///
    /// # Panics
    ///
    /// Panics if `data` exceeds the block's data capacity.
    pub fn set_data(&mut self, data: &[u8]) {
        assert!(
            data.len() <= self.capacity,
            "data ({}) exceeds block data capacity ({})",
            data.len(),
            self.capacity
        );
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr, data.len());
        }
        self.len = data.len();
    }

    /// Sets the valid data length (use after writing via `as_mut_slice`).
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity);
        self.len = len;
    }

    /// Maximum bytes this loan can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Publishes the block and wakes any blocked subscribers.
    /// O(1) — writes 8 bytes to the ring regardless of payload size.
    #[inline]
    pub fn publish(self) {
        self.commit(true);
        std::mem::forget(self); // block is published; don't run Drop
    }

    /// Publishes without waking subscribers. Saves ~170 ns by skipping
    /// the futex syscall.
    #[inline]
    pub fn publish_silent(self) {
        self.commit(false);
        std::mem::forget(self);
    }

    #[inline]
    fn commit(&self, wake: bool) {
        let ring_depth = self.region.config.ring_depth;
        let slot = (self.seq % ring_depth as u64) as u32;
        let entry_off = ring_entry_off(&self.region.config, self.topic_idx, slot);
        let entry_ptr = unsafe { self.region.mmap.as_mut_ptr().add(entry_off) };

        // 1. Read old block_idx from the ring slot we're about to overwrite
        let old_block_idx = unsafe { (entry_ptr.add(RE_BLOCK_IDX) as *const u32).read() };

        // 2. Set new block's refcount to 1 (ring holds one reference)
        self.region
            .block_refcount(self.block_idx)
            .store(1, Ordering::Release);

        // 3. Invalidate seq (seqlock open) — prevents subscribers from reading mid-write
        let entry_seq = unsafe { &*(entry_ptr.add(RE_SEQ) as *const AtomicU64) };
        entry_seq.store(0, Ordering::Release);

        // 4. Write new block_idx and data_len
        unsafe {
            (entry_ptr.add(RE_BLOCK_IDX) as *mut u32).write(self.block_idx);
            (entry_ptr.add(RE_DATA_LEN) as *mut u32).write(self.len as u32);
        }

        // 5. Validate seq (seqlock close) — data visible to subscribers
        entry_seq.store(self.seq, Ordering::Release);

        // 6. Bump topic write_seq
        self.write_seq_atom.store(self.seq, Ordering::Release);

        // 7. Release old block (decrement refcount; free if no subscribers hold it)
        if old_block_idx != NO_BLOCK {
            let prev = self
                .region
                .block_refcount(old_block_idx)
                .fetch_sub(1, Ordering::AcqRel);
            if prev == 1 {
                self.region.free_block(old_block_idx);
            }
        }

        // 8. Smart wake: always bump notification counter, but only issue the
        //    expensive futex_wake syscall (~170ns) when a subscriber is actually
        //    blocked in recv(). When all subscribers poll with try_recv(), this
        //    path costs ~8ns instead of ~170ns.
        if wake {
            self.notify_atom.fetch_add(1, Ordering::Release);
            if self.waiters_atom.load(Ordering::Acquire) > 0 {
                notify::wake_all(self.notify_atom);
            }
        }
    }
}

impl<'a> Drop for ShmPoolLoan<'a> {
    fn drop(&mut self) {
        // Loan dropped without publish — return block to pool
        self.region.free_block(self.block_idx);
    }
}

impl<'a> io::Write for ShmPoolLoan<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.capacity - self.len;
        if remaining == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "block full"));
        }
        let n = buf.len().min(remaining);
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), self.data_ptr.add(self.len), n);
        }
        self.len += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ─── ShmPoolSubscriber ───────────────────────────────────────────────────

/// O(1) zero-copy subscriber for pool-backed pub/sub.
///
/// Connects to an existing [`ShmPoolPublisher`] region. Returns safe
/// [`ShmPoolSampleGuard`] references — no `unsafe` needed to read data.
///
/// # Examples
///
/// ```rust,no_run
/// use crossbar::prelude::*;
///
/// let sub = ShmPoolSubscriber::connect("prices").unwrap();
/// let mut stream = sub.subscribe("/tick/AAPL").unwrap();
///
/// if let Some(sample) = stream.try_recv() {
///     println!("data: {:?}", &*sample); // safe Deref<Target=[u8]>
/// }
/// ```
pub struct ShmPoolSubscriber {
    region: Arc<PoolRegion>,
}

impl ShmPoolSubscriber {
    /// Connects to an existing pool pub/sub region.
    ///
    /// # Errors
    ///
    /// Returns an error if the region file doesn't exist, has invalid
    /// magic/version, or the publisher's heartbeat is stale.
    pub fn connect(name: &str) -> Result<Self, CrossbarError> {
        let path = shm_path(name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true) // needed for atomic CAS on refcounts
            .open(&path)
            .map_err(CrossbarError::Io)?;

        let mmap = RawMmap::from_file(&file).map_err(CrossbarError::Io)?;

        if mmap.len() < HEADER_SIZE {
            return Err(CrossbarError::ShmInvalidRegion(
                "region too small for header".into(),
            ));
        }

        // Validate header
        let ptr = mmap.as_ptr();
        unsafe {
            let mut magic = [0u8; 8];
            std::ptr::copy_nonoverlapping(ptr.add(GH_MAGIC), magic.as_mut_ptr(), 8);
            if &magic != MAGIC {
                return Err(CrossbarError::ShmInvalidRegion(
                    "invalid magic (expected XBAR_ZC)".into(),
                ));
            }
            let ver = (ptr.add(GH_VERSION) as *const u32).read();
            if ver != VERSION {
                return Err(CrossbarError::ShmInvalidRegion(format!(
                    "unsupported version {ver}, expected {VERSION}"
                )));
            }
        }

        // Read config from header
        let config = unsafe {
            PoolPubSubConfig {
                max_topics: (ptr.add(GH_MAX_TOPICS) as *const u32).read(),
                block_count: (ptr.add(GH_BLOCK_COUNT) as *const u32).read(),
                block_size: (ptr.add(GH_BLOCK_SIZE) as *const u32).read(),
                ring_depth: (ptr.add(GH_RING_DEPTH) as *const u32).read(),
                stale_timeout: Duration::from_micros(
                    (ptr.add(GH_STALE_TIMEOUT_US) as *const u64).read(),
                ),
                ..PoolPubSubConfig::default()
            }
        };

        let pool_off = block_pool_offset(&config);
        let expected_size = region_size(&config);
        if mmap.len() < expected_size {
            return Err(CrossbarError::ShmInvalidRegion(format!(
                "region size {} < expected {expected_size}",
                mmap.len()
            )));
        }

        let region = Arc::new(PoolRegion {
            mmap,
            config,
            pool_offset: pool_off,
        });

        region.check_heartbeat()?;

        Ok(ShmPoolSubscriber { region })
    }

    /// Subscribes to a topic by URI.
    ///
    /// # Errors
    ///
    /// Returns an error if the topic is not registered by the publisher.
    pub fn subscribe(&self, uri: &str) -> Result<ShmPoolSubscription, CrossbarError> {
        let hash = uri_hash(uri);

        for i in 0..self.region.config.max_topics {
            let off = topic_entry_off(i);
            let active =
                unsafe { &*(self.region.mmap.as_ptr().add(off + TE_ACTIVE) as *const AtomicU32) };
            if active.load(Ordering::Acquire) != 1 {
                continue;
            }
            let stored_hash =
                unsafe { (self.region.mmap.as_ptr().add(off + TE_URI_HASH) as *const u64).read() };
            if stored_hash != hash {
                continue;
            }
            // Verify URI bytes match (not just hash) to prevent collision aliasing
            let stored_len =
                unsafe { (self.region.mmap.as_ptr().add(off + TE_URI_LEN) as *const u32).read() }
                    as usize;
            if stored_len != uri.len() {
                continue;
            }
            let stored_bytes = unsafe {
                std::slice::from_raw_parts(self.region.mmap.as_ptr().add(off + TE_URI), stored_len)
            };
            if stored_bytes != uri.as_bytes() {
                continue;
            }

            let write_seq = unsafe {
                &*(self.region.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64)
            };
            // Start from latest — don't replay history
            let current = write_seq.load(Ordering::Acquire);

            return Ok(ShmPoolSubscription {
                region: Arc::clone(&self.region),
                topic_idx: i,
                last_seq: current,
            });
        }

        Err(CrossbarError::ShmInvalidRegion(format!(
            "topic '{uri}' not found"
        )))
    }
}

// ─── ShmPoolSubscription ─────────────────────────────────────────────────

/// A subscription to a single topic on a pool-backed pub/sub region.
///
/// Returns [`ShmPoolSampleGuard`] references that implement `Deref<Target=[u8]>`.
/// These guards are **safe** — the block is held alive by atomic refcounting
/// until the guard is dropped.
pub struct ShmPoolSubscription {
    region: Arc<PoolRegion>,
    topic_idx: u32,
    last_seq: u64,
}

impl ShmPoolSubscription {
    fn write_seq_atom(&self) -> &AtomicU64 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.mmap.as_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) }
    }

    fn notify_atom(&self) -> &AtomicU32 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.mmap.as_ptr().add(off + TE_NOTIFY) as *const AtomicU32) }
    }

    fn waiters_atom(&self) -> &AtomicU32 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.mmap.as_ptr().add(off + TE_WAITERS) as *const AtomicU32) }
    }

    /// Non-blocking: returns the next sample guard or `None`.
    ///
    /// The returned guard implements `Deref<Target=[u8]>` — safe to read
    /// without `unsafe`. The block is held alive until the guard is dropped.
    #[inline]
    pub fn try_recv(&mut self) -> Option<ShmPoolSampleGuard> {
        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq {
            return None;
        }

        let target = if current_seq - self.last_seq > self.region.config.ring_depth as u64 {
            current_seq // fell behind — skip to latest
        } else {
            self.last_seq + 1
        };

        if let Some(guard) = self.try_read_slot(target) {
            return Some(guard);
        }

        // Slot was overwritten — retry with latest
        let latest = self.write_seq_atom().load(Ordering::Acquire);
        if latest > self.last_seq && latest != target {
            self.try_read_slot(latest)
        } else {
            None
        }
    }

    #[inline]
    fn try_read_slot(&mut self, seq: u64) -> Option<ShmPoolSampleGuard> {
        let ring_depth = self.region.config.ring_depth;
        let slot = (seq % ring_depth as u64) as u32;
        let entry_off = ring_entry_off(&self.region.config, self.topic_idx, slot);
        let entry_ptr = unsafe { self.region.mmap.as_ptr().add(entry_off) };

        // Seqlock check 1
        let entry_seq = unsafe { &*(entry_ptr.add(RE_SEQ) as *const AtomicU64) };
        if entry_seq.load(Ordering::Acquire) != seq {
            return None;
        }

        // Read block_idx and data_len
        let block_idx = unsafe { (entry_ptr.add(RE_BLOCK_IDX) as *const u32).read() };
        let data_len = unsafe { (entry_ptr.add(RE_DATA_LEN) as *const u32).read() };

        if block_idx == NO_BLOCK {
            return None;
        }

        // Bounds check
        if data_len as usize > self.region.data_capacity() {
            return None;
        }

        // CAS increment refcount — acquire a reference to the block
        let refcount = self.region.block_refcount(block_idx);
        loop {
            let rc = refcount.load(Ordering::Acquire);
            if rc == 0 {
                return None; // block already freed
            }
            if refcount
                .compare_exchange(rc, rc + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        // Seqlock check 2 — verify ring slot wasn't overwritten during our read
        if entry_seq.load(Ordering::Acquire) != seq {
            // Undo refcount increment
            let prev = refcount.fetch_sub(1, Ordering::AcqRel);
            if prev == 1 {
                self.region.free_block(block_idx);
            }
            return None;
        }

        self.last_seq = seq;

        Some(ShmPoolSampleGuard {
            region: Arc::clone(&self.region),
            block_idx,
            len: data_len as usize,
        })
    }

    /// Blocking: waits for the next sample. Uses spin → futex.
    ///
    /// Returns `Err(ShmServerDead)` if the publisher heartbeat goes stale.
    pub fn recv(&mut self) -> Result<ShmPoolSampleGuard, CrossbarError> {
        let poll_ms = (self.region.config.stale_timeout.as_millis() as u64 / 3).clamp(1, 50);

        loop {
            if let Some(g) = self.try_recv() {
                return Ok(g);
            }

            let cur = self.notify_atom().load(Ordering::Acquire);

            // Re-check after loading counter
            if let Some(g) = self.try_recv() {
                return Ok(g);
            }

            // Signal that we're about to sleep — publisher checks this to
            // decide whether futex_wake is needed (smart wake).
            self.waiters_atom().fetch_add(1, Ordering::AcqRel);

            // Re-check after incrementing waiters to avoid missed-wake:
            // a publish between last try_recv and waiters increment would
            // have skipped futex_wake (saw waiters=0).
            if let Some(g) = self.try_recv() {
                self.waiters_atom().fetch_sub(1, Ordering::Release);
                return Ok(g);
            }

            notify::wait_until_not(self.notify_atom(), cur, Duration::from_millis(poll_ms)).ok();
            self.waiters_atom().fetch_sub(1, Ordering::Release);
            self.region.check_heartbeat()?;
        }
    }
}

// ─── ShmPoolSampleGuard ──────────────────────────────────────────────────

/// Safe zero-copy reference to a published sample in shared memory.
///
/// Implements `Deref<Target=[u8]>` — you can read the data without `unsafe`.
/// The underlying pool block is held alive by an atomic refcount and freed
/// back to the pool when this guard (and all clones/copies) are dropped.
///
/// # Safety (internal)
///
/// This is safe because:
/// 1. The mmap is kept alive via `Arc<PoolRegion>`.
/// 2. The block cannot be freed while refcount > 0.
/// 3. No writer touches the block's data region after publishing.
/// 4. The data region does not overlap the free-list link field.
pub struct ShmPoolSampleGuard {
    region: Arc<PoolRegion>,
    block_idx: u32,
    len: usize,
}

// SAFETY: The guard only reads immutable data in the mmap. The refcount
// prevents the block from being freed or reallocated while the guard exists.
unsafe impl Send for ShmPoolSampleGuard {}
unsafe impl Sync for ShmPoolSampleGuard {}

impl ShmPoolSampleGuard {
    /// Returns the data length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the sample has zero length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns a pointer to the data. Prefer `Deref` instead.
    pub fn as_ptr(&self) -> *const u8 {
        unsafe { self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET) }
    }

    /// Copies the data into a new `Vec<u8>`.
    pub fn to_vec(&self) -> Vec<u8> {
        self.deref().to_vec()
    }
}

impl Deref for ShmPoolSampleGuard {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            std::slice::from_raw_parts(ptr, self.len)
        }
    }
}

impl AsRef<[u8]> for ShmPoolSampleGuard {
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

impl std::fmt::Debug for ShmPoolSampleGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmPoolSampleGuard")
            .field("block_idx", &self.block_idx)
            .field("len", &self.len)
            .finish()
    }
}

impl Drop for ShmPoolSampleGuard {
    fn drop(&mut self) {
        let refcount = self.region.block_refcount(self.block_idx);
        let prev = refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.region.free_block(self.block_idx);
        }
    }
}
