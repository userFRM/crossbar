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
    base: *mut u8,
    pub(crate) config: PubSubConfig,
    pub(crate) pool_offset: usize,
    pub(crate) last_freed: AtomicU32,
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
    pub(crate) unsafe fn from_raw(base: *mut u8, _len: usize, config: PubSubConfig) -> Self {
        let pool_offset = block_pool_offset(&config);
        Self {
            base,
            config,
            pool_offset,
            last_freed: AtomicU32::new(NO_BLOCK),
        }
    }

    /// Returns the raw base pointer to the mmap region.
    #[inline]
    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    #[inline]
    pub(crate) fn pool_head(&self) -> &AtomicU64 {
        unsafe { &*(self.base.add(GH_POOL_HEAD) as *const AtomicU64) }
    }

    /// Returns a pointer to the block at `idx`, or `None` if out of bounds.
    #[inline]
    pub(crate) fn block_ptr_checked(&self, idx: u32) -> Option<*mut u8> {
        if (idx as usize) >= self.config.block_count as usize {
            return None;
        }
        Some(unsafe {
            self.base
                .add(self.pool_offset + idx as usize * self.config.block_size as usize)
        })
    }

    /// Returns a pointer to the block at `idx`.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if `idx >= block_count`.
    #[inline]
    pub(crate) fn block_ptr(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.config.block_count as usize);
        unsafe {
            self.base
                .add(self.pool_offset + idx as usize * self.config.block_size as usize)
        }
    }

    /// Returns the refcount atomic for block `idx`, with bounds check.
    /// Returns `None` if `idx >= block_count`.
    #[inline]
    pub(crate) fn block_refcount_checked(&self, idx: u32) -> Option<&AtomicU32> {
        let ptr = self.block_ptr_checked(idx)?;
        Some(unsafe { &*(ptr.add(BK_REFCOUNT) as *const AtomicU32) })
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
            // Validate idx is within bounds (H6: free-list corruption defense)
            if (idx as usize) >= self.config.block_count as usize {
                return None;
            }
            let block = self.block_ptr(idx);
            let next = unsafe { &*(block as *const AtomicU32) }.load(Ordering::Relaxed);

            // Validate next pointer too (defense against corrupted free-list)
            if next != NO_BLOCK && (next as usize) >= self.config.block_count as usize {
                return None;
            }

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

    /// Try to grab the recycled block (cache-warm from the last publish).
    /// Returns `Some(idx)` if a recycled block was available, `None` otherwise.
    #[inline]
    pub(crate) fn alloc_recycled(&self) -> Option<u32> {
        let idx = self.last_freed.swap(NO_BLOCK, Ordering::AcqRel);
        if idx != NO_BLOCK {
            Some(idx)
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn free_block(&self, idx: u32) {
        if (idx as usize) >= self.config.block_count as usize {
            return; // Bounds check: silently ignore corrupt block_idx in Drop paths
        }
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

    fn heartbeat_atom(&self) -> &AtomicU64 {
        unsafe { &*(self.base.add(GH_HEARTBEAT) as *const AtomicU64) }
    }

    /// Returns microseconds since UNIX epoch, or error if clock is behind epoch.
    #[cfg(feature = "std")]
    #[allow(clippy::cast_possible_truncation)]
    fn now_micros() -> Result<u64, crate::error::IpcError> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .map_err(|_| crate::error::IpcError::ClockError)
    }

    #[cfg(feature = "std")]
    pub(crate) fn update_heartbeat(&self) -> Result<(), crate::error::IpcError> {
        let now = Self::now_micros()?;
        self.heartbeat_atom().fetch_max(now, Ordering::Release);
        Ok(())
    }

    #[cfg(feature = "std")]
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn check_heartbeat(&self) -> Result<(), crate::error::IpcError> {
        let hb = self.heartbeat_atom().load(Ordering::Acquire);
        let now = Self::now_micros()?;
        if now.saturating_sub(hb) > self.config.stale_timeout.as_micros() as u64 {
            return Err(crate::error::IpcError::PublisherDead);
        }
        Ok(())
    }

    /// Pinned publish: store (seq, data_len) atomically. The block is permanently
    /// assigned -- no alloc, no free, no refcount. 1 atomic Release store.
    #[cfg(feature = "std")]
    #[inline]
    pub(crate) fn commit_pinned(
        &self,
        data_len: u32,
        topic_idx: u32,
        write_seq_atom: &AtomicU64,
        waiters_atom: &AtomicU32,
    ) {
        // Claim sequence number (still needed for subscriber to detect new data)
        let seq = write_seq_atom.fetch_add(1, Ordering::AcqRel) + 1;

        // Store packed (seq:32 | data_len:32) in the pinned seqlock field.
        // The Release ordering ensures all data writes are visible before this store.
        let off = topic_entry_off(topic_idx);
        let pinned_seq = unsafe { &*(self.base.add(off + TE_PINNED_SEQ) as *const AtomicU64) };
        let packed = (seq & 0xFFFF_FFFF) << 32 | u64::from(data_len);
        pinned_seq.store(packed, Ordering::Release);

        // Smart wake: use write_seq low-32 as futex address (same as regular path)
        if waiters_atom.load(Ordering::Acquire) > 0 {
            let seq_futex = unsafe { &*(write_seq_atom as *const AtomicU64 as *const AtomicU32) };
            notify::wake_all(seq_futex);
        }
    }

    #[inline]
    pub(crate) fn set_pinned_block(&self, topic_idx: u32, block_idx: u32) {
        let off = topic_entry_off(topic_idx);
        // Use atomic store with Release so subscriber's Acquire load sees the value
        unsafe { &*(self.base.add(off + TE_PINNED_BLOCK) as *const AtomicU32) }
            .store(block_idx, Ordering::Release);
    }

    /// Load pinned block index atomically.
    #[inline]
    pub(crate) fn pinned_block(&self, topic_idx: u32) -> u32 {
        let off = topic_entry_off(topic_idx);
        unsafe { &*(self.base.add(off + TE_PINNED_BLOCK) as *const AtomicU32) }
            .load(Ordering::Acquire)
    }

    /// Returns the pinned readers atomic for a topic.
    #[inline]
    pub(crate) fn pinned_readers(&self, topic_idx: u32) -> &AtomicU32 {
        let off = topic_entry_off(topic_idx);
        unsafe { &*(self.base.add(off + TE_PINNED_READERS) as *const AtomicU32) }
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

        // 7. Release old block (decrement refcount; recycle if no subscribers hold it)
        if old_block_idx != NO_BLOCK && (old_block_idx as usize) < self.config.block_count as usize
        {
            let prev = self
                .block_refcount(old_block_idx)
                .fetch_sub(1, Ordering::AcqRel);
            if prev == 1 {
                // Recycle this block for warm reuse by the next alloc.
                // If a previous recycled block wasn't claimed, free it to the pool.
                let old_recycled = self.last_freed.swap(old_block_idx, Ordering::AcqRel);
                if old_recycled != NO_BLOCK {
                    self.free_block(old_recycled);
                }
            }
        }

        // 8. Smart wake: only issue the expensive futex_wake syscall (~170ns)
        //    when a subscriber is actually blocked in recv(). Reinterpret the
        //    low 32 bits of write_seq as the futex address -- the fetch_add
        //    already changed the value so waiters will see it and wake up.
        if wake && waiters_atom.load(Ordering::Acquire) > 0 {
            // Safety: AtomicU64 is at least 4-byte aligned. On little-endian
            // (enforced by compile_error in layout.rs), the low 32 bits are
            // at the base address.
            let seq_futex = unsafe { &*(write_seq_atom as *const AtomicU64 as *const AtomicU32) };
            notify::wake_all(seq_futex);
        }
    }
}

/// Release a block's refcount. If this is the last reference, free it to the pool.
/// Shared by `SampleGuard::drop`, `TypedSampleGuard::drop`, and `CrossbarSample::drop`.
pub(crate) fn release_block(region: &Region, block_idx: u32) {
    // Runtime bounds check (Codex/Security: must not use debug_assert in Drop paths)
    let refcount = match region.block_refcount_checked(block_idx) {
        Some(rc) => rc,
        None => return, // OOB block_idx — silently ignore in Drop path
    };
    let prev = refcount.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        core::sync::atomic::fence(Ordering::Acquire);
        region.free_block(block_idx);
    }
}
