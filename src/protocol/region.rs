// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Region: shared state for the mmap region.
//!
//! Works on raw pointers -- no OS dependency. The platform layer constructs
//! a `Region` from an mmap'd pointer.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::config::PubSubConfig;
use super::layout::*;

#[cfg(feature = "std")]
use crate::platform::notify;

/// Shared state for the mmap region -- held by both publisher/subscriber
/// and by `SampleGuard` (via `Arc`) to keep the mmap alive.
pub struct Region {
    pub(crate) base: *mut u8,
    #[allow(dead_code)]
    pub(crate) len: usize,
    pub(crate) config: PubSubConfig,
    pub(crate) pool_offset: usize,
}

// SAFETY: The mmap region is process-shared memory backed by a named file in
// /dev/shm. All cross-process access is mediated by atomic operations in the
// caller. The raw pointer is never dereferenced without explicit unsafe blocks.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// Construct a Region from a raw pointer and length.
    ///
    /// # Safety
    ///
    /// `base` must point to a valid, mmap'd region of at least `len` bytes
    /// that remains valid for the lifetime of this Region.
    pub unsafe fn from_raw(base: *mut u8, len: usize, config: PubSubConfig) -> Self {
        let pool_offset = block_pool_offset(&config);
        Self {
            base,
            len,
            config,
            pool_offset,
        }
    }

    #[inline]
    pub(crate) fn pool_head(&self) -> &AtomicU64 {
        unsafe { &*(self.base.add(GH_POOL_HEAD) as *const AtomicU64) }
    }

    #[inline]
    pub(crate) fn block_ptr(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        unsafe {
            self.base
                .add(self.pool_offset + idx as usize * self.config.block_size as usize)
        }
    }

    #[inline]
    pub(crate) fn block_refcount(&self, idx: u32) -> &AtomicU32 {
        unsafe { &*(self.block_ptr(idx).add(BK_REFCOUNT) as *const AtomicU32) }
    }

    #[inline]
    pub(crate) fn alloc_block(&self) -> Option<u32> {
        let mut head = self.pool_head().load(Ordering::Acquire);
        loop {
            let (gen, idx) = unpack(head);
            if idx == NO_BLOCK {
                return None;
            }
            let block = self.block_ptr(idx);
            let next = unsafe { &*(block as *const AtomicU32) }.load(Ordering::Relaxed);
            let new_head = pack(gen.wrapping_add(1), next);

            // Prefetch the block data into L1 cache before the CAS attempt
            #[cfg(target_arch = "x86_64")]
            unsafe {
                core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                    block as *const i8,
                );
            }
            #[cfg(target_arch = "aarch64")]
            unsafe {
                core::arch::asm!(
                    "prfm pstl1keep, [{addr}]",
                    addr = in(reg) block,
                    options(nostack, preserves_flags)
                );
            }

            match self.pool_head().compare_exchange_weak(
                head,
                new_head,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(idx),
                Err(current) => head = current,
            }
        }
    }

    #[inline]
    pub(crate) fn free_block(&self, idx: u32) {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        let mut head = self.pool_head().load(Ordering::Acquire);
        loop {
            let (gen, old_head_idx) = unpack(head);
            // Write next pointer into the block's free-list link (offset 0)
            unsafe { &*(self.block_ptr(idx) as *const AtomicU32) }
                .store(old_head_idx, Ordering::Relaxed);
            let new_head = pack(gen.wrapping_add(1), idx);
            match self.pool_head().compare_exchange_weak(
                head,
                new_head,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }

    pub(crate) fn init_free_list(&self) {
        for i in 0..self.config.block_count {
            let next = if i + 1 < self.config.block_count {
                i + 1
            } else {
                NO_BLOCK
            };
            let ptr = self.block_ptr(i);
            unsafe { &*(ptr as *const AtomicU32) }.store(next, Ordering::Relaxed);
            // Zero refcount
            unsafe { &*(ptr.add(BK_REFCOUNT) as *const AtomicU32) }.store(0, Ordering::Relaxed);
        }
        self.pool_head().store(pack(0, 0), Ordering::Release);
    }

    /// Data capacity per block (block_size minus the 8-byte header).
    pub(crate) fn data_capacity(&self) -> usize {
        self.config.block_size as usize - BLOCK_DATA_OFFSET
    }

    pub(crate) fn heartbeat_atom(&self) -> &AtomicU64 {
        unsafe { &*(self.base.add(GH_HEARTBEAT) as *const AtomicU64) }
    }

    #[cfg(feature = "std")]
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn update_heartbeat(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        self.heartbeat_atom().fetch_max(now, Ordering::Release);
    }

    #[cfg(feature = "std")]
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn check_heartbeat(&self) -> Result<(), crate::error::IpcError> {
        let hb = self.heartbeat_atom().load(Ordering::Acquire);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        if now.saturating_sub(hb) > self.config.stale_timeout.as_micros() as u64 {
            return Err(crate::error::IpcError::PublisherDead);
        }
        Ok(())
    }

    /// Shared commit logic for both `ShmLoan` and `TypedShmLoan`.
    ///
    /// Atomically claims the next sequence number via `fetch_add` on
    /// `write_seq_atom`, then uses CAS-based seqlock to prevent two
    /// publishers from writing the same ring slot simultaneously.
    #[cfg(feature = "std")]
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_to_ring(
        &self,
        block_idx: u32,
        data_len: u32,
        topic_idx: u32,
        write_seq_atom: &AtomicU64,
        waiters_atom: &AtomicU32,
        wake: bool,
        single_publisher: bool,
    ) {
        // 1. Atomically claim the next sequence number
        let seq = write_seq_atom.fetch_add(1, Ordering::AcqRel) + 1;

        let ring_mask = self.config.ring_depth as u64 - 1;
        let slot = (seq & ring_mask) as u32;
        let entry_off = ring_entry_off(&self.config, topic_idx, slot);
        let entry_ptr = unsafe { self.base.add(entry_off) };
        let entry_seq = unsafe { &*(entry_ptr.add(RE_SEQ) as *const AtomicU64) };

        // 2. Acquire the ring slot via CAS (prevents two publishers from
        //    writing the same slot when seqs differ by exactly ring_depth).
        //    Single-publisher mode skips the CAS loop entirely.
        if single_publisher {
            entry_seq.store(SEQ_WRITING, Ordering::Release);
        } else {
            loop {
                let current = entry_seq.load(Ordering::Acquire);
                if current == SEQ_WRITING {
                    crate::wait::yield_hint();
                    continue;
                }
                if entry_seq
                    .compare_exchange_weak(
                        current,
                        SEQ_WRITING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
                crate::wait::yield_hint();
            }
        }

        // 3. Read old block_idx from the slot we're overwriting
        let old_block_idx =
            unsafe { &*(entry_ptr.add(RE_BLOCK_IDX) as *const AtomicU32) }.load(Ordering::Relaxed);

        // 4. Set new block's refcount to 1
        self.block_refcount(block_idx).store(1, Ordering::Release);

        // 5. Write new block_idx and data_len
        unsafe { &*(entry_ptr.add(RE_BLOCK_IDX) as *const AtomicU32) }
            .store(block_idx, Ordering::Relaxed);
        unsafe { &*(entry_ptr.add(RE_DATA_LEN) as *const AtomicU32) }
            .store(data_len, Ordering::Relaxed);

        // 6. Seqlock close -- data visible to subscribers
        entry_seq.store(seq, Ordering::Release);

        // 7. Release old block (decrement refcount; free if no subscribers hold it)
        if old_block_idx != NO_BLOCK {
            let prev = self
                .block_refcount(old_block_idx)
                .fetch_sub(1, Ordering::AcqRel);
            if prev == 1 {
                self.free_block(old_block_idx);
            }
        }

        // 8. Smart wake: only issue the expensive futex_wake syscall (~170ns)
        //    when a subscriber is actually blocked in recv(). Reinterpret the
        //    low 32 bits of write_seq as the futex address -- the fetch_add
        //    already changed the value so waiters will see it and wake up.
        if wake && waiters_atom.load(Ordering::Acquire) > 0 {
            // Safety: AtomicU64 is at least 4-byte aligned. On little-endian
            // (all supported platforms), the low 32 bits are at the base address.
            let seq_futex = unsafe { &*(write_seq_atom as *const AtomicU64 as *const AtomicU32) };
            notify::wake_all(seq_futex);
        }
    }
}
