// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Stream, Sample, TypedSample.

use std::cell::Cell;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::Error;
use crate::protocol::layout::*;
use crate::protocol::{release_block, Region};
use crate::wait::WaitStrategy;

use super::notify;

// ---- Stream ----

/// A subscription to a single topic on a pool-backed pub/sub region.
///
/// Returns [`Sample`] references that implement `Deref<Target=[u8]>`.
/// These guards are **safe** -- the block is held alive by atomic refcounting
/// until the guard is dropped.
pub struct Stream {
    pub(crate) region: Arc<Region>,
    pub(crate) topic_idx: u32,
    pub(crate) last_seq: Cell<u64>,
    pub(crate) type_size: u32,
}

impl core::fmt::Debug for Stream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Stream")
            .field("topic_idx", &self.topic_idx)
            .field("type_size", &self.type_size)
            .field("last_seq", &self.last_seq.get())
            .finish()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let off = topic_entry_off(self.topic_idx);
        let counter = unsafe {
            &*(self.region.base_ptr().add(off + TE_SUBSCRIBER_COUNT) as *const AtomicU32)
        };
        counter.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Stream {
    fn write_seq_atom(&self) -> &AtomicU64 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.base_ptr().add(off + TE_WRITE_SEQ) as *const AtomicU64) }
    }

    fn waiters_atom(&self) -> &AtomicU32 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.base_ptr().add(off + TE_WAITERS) as *const AtomicU32) }
    }

    /// Non-blocking: returns the next sample guard or `None`.
    ///
    /// The returned guard implements `Deref<Target=[u8]>` -- safe to read
    /// without `unsafe`. The block is held alive until the guard is dropped.
    ///
    /// Uses a ring-window scan to handle multi-publisher scenarios where
    /// sequence numbers are claimed atomically and slots may be committed
    /// out of order.
    #[inline]
    pub fn try_recv(&self) -> Option<Sample<'_>> {
        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq.get() {
            return None;
        }

        let ring_depth = self.region.config.ring_depth as u64;
        let start = self.last_seq.get() + 1;

        // If we've fallen far behind, skip to the recent window
        let scan_start = if current_seq - start >= ring_depth {
            current_seq - ring_depth + 1
        } else {
            start
        };

        // Scan for the first committed slot in the window
        for seq in scan_start..=current_seq {
            if let Some(guard) = self.try_read_slot(seq) {
                return Some(guard);
            }
        }

        None
    }

    /// Raw slot read: performs seqlock + refcount CAS and returns slot metadata.
    /// Shared by both untyped and typed recv paths.
    #[inline]
    fn try_read_slot_raw(&self, seq: u64) -> Option<SlotRead> {
        let ring_mask = self.region.config.ring_depth as u64 - 1;
        let slot = (seq & ring_mask) as u32;
        let entry_off = ring_entry_off(&self.region.config, self.topic_idx, slot);
        let entry_ptr = unsafe { self.region.base_ptr().add(entry_off) };

        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                entry_ptr as *const i8,
            );
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!(
                "prfm pldl1keep, [{addr}]",
                addr = in(reg) entry_ptr,
                options(nostack, preserves_flags)
            );
        }

        // Seqlock check 1
        let entry_seq = unsafe { &*(entry_ptr.add(RE_SEQ) as *const AtomicU64) };
        if entry_seq.load(Ordering::Acquire) != seq {
            return None;
        }

        // Read block_idx and data_len
        let block_idx =
            unsafe { &*(entry_ptr.add(RE_BLOCK_IDX) as *const AtomicU32) }.load(Ordering::Relaxed);
        let data_len =
            unsafe { &*(entry_ptr.add(RE_DATA_LEN) as *const AtomicU32) }.load(Ordering::Relaxed);

        if block_idx == NO_BLOCK {
            return None;
        }

        // C1: Runtime bounds check on block_idx (defense against malicious SHM)
        if (block_idx as usize) >= self.region.config.block_count as usize {
            return None;
        }

        // Bounds check on data_len
        if data_len as usize > self.region.data_capacity() {
            return None;
        }

        // CAS increment refcount -- acquire a reference to the block
        // Use block_refcount_checked for extra safety
        let refcount = self.region.block_refcount_checked(block_idx)?;
        loop {
            let rc = refcount.load(Ordering::Acquire);
            if rc == 0 {
                return None; // block already freed
            }
            if refcount
                .compare_exchange_weak(rc, rc + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        // Seqlock check 2 -- verify ring slot wasn't overwritten during our read
        if entry_seq.load(Ordering::Acquire) != seq {
            // Undo refcount increment
            release_block(&self.region, block_idx);
            return None;
        }

        self.last_seq.set(seq);

        Some(SlotRead {
            block_idx,
            data_len,
        })
    }

    #[inline]
    fn try_read_slot(&self, seq: u64) -> Option<Sample<'_>> {
        let slot = self.try_read_slot_raw(seq)?;
        Some(Sample {
            region: &self.region,
            block_idx: slot.block_idx,
            len: slot.data_len as usize,
        })
    }

    /// Non-blocking typed receive. Returns a [`TypedSample`] that
    /// dereferences to `&T`.
    ///
    /// Uses the same ring-window scan as [`try_recv`](Self::try_recv) to
    /// handle multi-publisher scenarios.
    ///
    /// Returns `None` if no new data is available or if the topic's
    /// registered type size doesn't match `size_of::<T>()`.
    pub fn try_recv_typed<T: crate::Pod>(&self) -> Option<TypedSample<'_, T>> {
        if self.type_size != 0 && self.type_size as usize != core::mem::size_of::<T>() {
            return None; // Type size mismatch — return None instead of panicking
        }

        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq.get() {
            return None;
        }

        let ring_depth = self.region.config.ring_depth as u64;
        let start = self.last_seq.get() + 1;

        // If we've fallen far behind, skip to the recent window
        let scan_start = if current_seq - start >= ring_depth {
            current_seq - ring_depth + 1
        } else {
            start
        };

        // Scan for the first committed slot in the window
        for seq in scan_start..=current_seq {
            if let Some(slot) = self.try_read_slot_raw(seq) {
                return Some(TypedSample {
                    region: &self.region,
                    block_idx: slot.block_idx,
                    _marker: core::marker::PhantomData,
                });
            }
        }

        None
    }

    /// Blocking typed receive with the default [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_typed<T: crate::Pod>(&self) -> Result<TypedSample<'_, T>, crate::error::Error> {
        self.recv_typed_with::<T>(crate::WaitStrategy::default())
    }

    /// Blocking typed receive with a custom [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_typed_with<T: crate::Pod>(
        &self,
        strategy: crate::WaitStrategy,
    ) -> Result<TypedSample<'_, T>, crate::error::Error> {
        self.blocking_recv(strategy, || self.try_recv_typed::<T>())
    }

    /// Blocking: waits for the next sample using the given [`WaitStrategy`].
    ///
    /// For `BusySpin`/`YieldSpin`/`BackoffSpin`: loops calling `try_recv()`
    /// with the strategy's wait hint, checking heartbeat periodically.
    ///
    /// For `Adaptive`: uses spin/yield phases, then falls through to the
    /// OS-assisted sleep path (`futex`/`WaitOnAddress`/`WFE`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_with(&self, strategy: WaitStrategy) -> Result<Sample<'_>, Error> {
        self.blocking_recv(strategy, || self.try_recv())
    }

    /// Blocking: waits for the next sample using the default [`WaitStrategy`]
    /// (three-phase adaptive: spin -> yield -> OS sleep).
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv(&self) -> Result<Sample<'_>, Error> {
        self.recv_with(WaitStrategy::default())
    }

    /// Non-blocking pinned receive. Returns a zero-overhead guard pointing
    /// directly into the pinned block.
    ///
    /// The guard increments a shared reader count on creation and decrements
    /// on drop. The publisher checks this count before writing -- if any
    /// readers exist, `loan_pinned` returns an error instead of causing UB.
    ///
    /// Returns `None` if no new data has been published since the last recv.
    pub fn try_recv_pinned(&self) -> Option<PinnedGuard<'_>> {
        // Load the pinned seqlock: packed (seq:32 | data_len:32)
        let off = topic_entry_off(self.topic_idx);
        let pinned_seq =
            unsafe { &*(self.region.base_ptr().add(off + TE_PINNED_SEQ) as *const AtomicU64) };
        let packed = pinned_seq.load(Ordering::Acquire);

        if packed == 0 {
            return None; // Never published in pinned mode
        }

        #[allow(clippy::cast_possible_truncation)]
        let seq = (packed >> 32) as u64;
        let data_len = packed as u32;

        if seq <= self.last_seq.get() {
            return None; // No new data
        }

        // Read pinned block index atomically (SHM-R1 fix: use atomic load)
        let block_idx = self.region.pinned_block(self.topic_idx);
        if block_idx == NO_BLOCK {
            return None;
        }

        // C1: Bounds check on block_idx from SHM
        if (block_idx as usize) >= self.region.config.block_count as usize {
            return None;
        }

        // CAS-based reader registration: only enter if no writer sentinel is set.
        // PINNED_WRITER_ACTIVE means the publisher is writing — do not enter.
        let readers = self.region.pinned_readers(self.topic_idx);
        loop {
            let current = readers.load(Ordering::Acquire);
            if current == PINNED_WRITER_ACTIVE {
                return None; // Publisher is writing — cannot read
            }
            // CAS: increment reader count, failing if writer entered between load and CAS
            match readers.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue, // Retry — another subscriber or writer raced us
            }
        }

        // Seqlock re-check: verify the data hasn't changed since we read packed.
        // If the publisher wrote new data between our packed load and reader
        // increment, our data_len/seq are stale.
        let packed2 = pinned_seq.load(Ordering::Acquire);
        if packed2 != packed {
            // Data changed — undo reader increment and return None
            readers.fetch_sub(1, Ordering::AcqRel);
            return None;
        }

        self.last_seq.set(seq);

        let data_ptr = unsafe { self.region.block_ptr(block_idx).add(BLOCK_DATA_OFFSET) };

        Some(PinnedGuard {
            data_ptr,
            len: data_len as usize,
            readers,
        })
    }

    /// Blocking pinned receive. Waits for the next pinned publish using
    /// the default [`WaitStrategy`] (three-phase adaptive: spin -> yield
    /// -> OS sleep).
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_pinned(&self) -> Result<PinnedGuard<'_>, Error> {
        self.recv_pinned_with(WaitStrategy::default())
    }

    /// Blocking pinned receive with a custom [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_pinned_with(&self, strategy: WaitStrategy) -> Result<PinnedGuard<'_>, Error> {
        self.blocking_recv(strategy, || self.try_recv_pinned())
    }

    /// Generic blocking receive loop shared by all blocking recv variants (M1).
    /// Eliminates the copy-paste between recv_with, recv_typed_with, and recv_pinned_with.
    fn blocking_recv<T>(
        &self,
        strategy: WaitStrategy,
        try_fn: impl Fn() -> Option<T>,
    ) -> Result<T, Error> {
        // Fast path: check immediately
        if let Some(g) = try_fn() {
            return Ok(g);
        }

        let poll_ms = (self.region.config.stale_timeout.as_millis() as u64 / 3).clamp(1, 50);
        let mut iter: u32 = 0;

        loop {
            if let Some(g) = try_fn() {
                return Ok(g);
            }

            match strategy {
                WaitStrategy::Adaptive {
                    spin_iters,
                    yield_iters,
                } if iter >= spin_iters.saturating_add(yield_iters) => {
                    // Phase 3: OS sleep -- use low-32 of write_seq as futex word
                    let seq_futex = unsafe {
                        &*(self.write_seq_atom() as *const AtomicU64 as *const AtomicU32)
                    };
                    let cur = seq_futex.load(Ordering::Acquire);
                    if let Some(g) = try_fn() {
                        return Ok(g);
                    }
                    self.waiters_atom().fetch_add(1, Ordering::Release);
                    let result = notify::wait_until_not(
                        seq_futex,
                        cur,
                        Duration::from_millis(poll_ms),
                        true,
                    );
                    self.waiters_atom().fetch_sub(1, Ordering::Release);

                    // Check heartbeat on timeout
                    if result.is_err() {
                        self.region.check_heartbeat()?;
                    }
                }
                _ => {
                    strategy.wait(iter);
                    iter = iter.saturating_add(1);

                    // Periodic heartbeat check for non-OS-sleep strategies
                    if iter.is_multiple_of(1024) {
                        self.region.check_heartbeat()?;
                    }
                }
            }
        }
    }
}

/// Raw slot data returned by `try_read_slot_raw`.
struct SlotRead {
    block_idx: u32,
    data_len: u32,
}

// ---- Sample ----

/// Safe zero-copy reference to a published sample in shared memory.
///
/// Implements `Deref<Target=[u8]>` -- you can read the data without `unsafe`.
/// The underlying pool block is held alive by an atomic refcount and freed
/// back to the pool when this guard (and all clones/copies) are dropped.
///
/// # Safety (internal)
///
/// This is safe because:
/// 1. The mmap is kept alive via the borrowed `&Region` (which itself is
///    inside an `Arc<Region>` held by the `Stream`).
/// 2. The block cannot be freed while refcount > 0.
/// 3. No writer touches the block's data region after publishing.
/// 4. The data region does not overlap the free-list link field.
pub struct Sample<'a> {
    pub(crate) region: &'a Region,
    pub(crate) block_idx: u32,
    pub(crate) len: usize,
}

impl Sample<'_> {
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

    /// Read a `T: Pod` header from the start of the sample.
    ///
    /// Returns a reference pointing directly into shared memory -- zero copy.
    ///
    /// Returns `None` if the sample is smaller than `size_of::<T>()` or the
    /// pointer is not properly aligned for `T`.
    pub fn read_header<T: crate::Pod>(&self) -> Option<&T> {
        if self.len < core::mem::size_of::<T>() {
            return None;
        }
        // Check alignment
        let ptr = self.as_ptr();
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        Some(unsafe { &*(ptr as *const T) })
    }

    /// Read a `[E: Pod]` array following a `T: Pod` header.
    ///
    /// The array starts at offset `size_of::<T>()` and the entry count
    /// is stored as a little-endian u32 at the end of the data.
    ///
    /// Returns a slice pointing directly into shared memory -- zero copy.
    ///
    /// Returns `None` if the layout is invalid (too small, misaligned, or
    /// the stored entry count doesn't match the available bytes).
    pub fn read_array<T: crate::Pod, E: crate::Pod>(&self) -> Option<&[E]> {
        let header_size = core::mem::size_of::<T>();
        let entry_size = core::mem::size_of::<E>();

        // Need at least header + 4 bytes for count
        if self.len < header_size + 4 {
            return None;
        }

        // Read entry count from the last 4 bytes
        let count_offset = self.len - 4;
        let data = &**self; // Deref to &[u8]
        let count =
            u32::from_le_bytes(data[count_offset..count_offset + 4].try_into().ok()?) as usize;

        // Verify the array fits between header and count
        let array_bytes = count * entry_size;
        if header_size + array_bytes + 4 != self.len {
            return None;
        }

        // Check alignment
        let array_ptr = unsafe { self.as_ptr().add(header_size) };
        if !(array_ptr as usize).is_multiple_of(core::mem::align_of::<E>()) {
            return None;
        }

        Some(unsafe { core::slice::from_raw_parts(array_ptr as *const E, count) })
    }
}

impl Deref for Sample<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            core::slice::from_raw_parts(ptr, self.len)
        }
    }
}

impl AsRef<[u8]> for Sample<'_> {
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

impl core::fmt::Debug for Sample<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sample").field("len", &self.len).finish()
    }
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        release_block(self.region, self.block_idx);
    }
}

// ---- TypedSample ----

/// Typed zero-copy reference to a `T: Pod` value in shared memory.
///
/// Implements `Deref<Target=T>` -- safe to read without `unsafe`.
/// The underlying pool block is held alive by an atomic refcount and freed
/// back to the pool when this guard is dropped.
pub struct TypedSample<'a, T: crate::Pod> {
    region: &'a Region,
    block_idx: u32,
    _marker: core::marker::PhantomData<&'a T>,
}

impl<T: crate::Pod> core::ops::Deref for TypedSample<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            &*(ptr as *const T)
        }
    }
}

impl<T: crate::Pod> Drop for TypedSample<'_, T> {
    fn drop(&mut self) {
        release_block(self.region, self.block_idx);
    }
}

impl<T: crate::Pod> core::fmt::Debug for TypedSample<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TypedSample")
            .field("size", &core::mem::size_of::<T>())
            .finish()
    }
}

// ---- PinnedGuard ----

/// Near-zero-overhead read reference to a pinned block in shared memory.
///
/// On creation, increments a shared reader count in the topic entry.
/// On drop, decrements it. The publisher checks this count before writing
/// and returns an error if any readers exist -- preventing data races at
/// runtime without requiring `unsafe`.
pub struct PinnedGuard<'a> {
    data_ptr: *const u8,
    len: usize,
    readers: &'a AtomicU32,
}

impl PinnedGuard<'_> {
    /// Returns the data length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the sample has zero length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for PinnedGuard<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.data_ptr, self.len) }
    }
}

impl AsRef<[u8]> for PinnedGuard<'_> {
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

impl Drop for PinnedGuard<'_> {
    fn drop(&mut self) {
        self.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

impl core::fmt::Debug for PinnedGuard<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PinnedGuard")
            .field("len", &self.len)
            .finish()
    }
}
