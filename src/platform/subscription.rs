// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subscription, SampleGuard, TypedSampleGuard.

use alloc::vec::Vec;
use std::cell::Cell;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::IpcError;
use crate::protocol::layout::*;
use crate::protocol::Region;
use crate::wait::WaitStrategy;

use super::notify;

// ---- Subscription ----

/// A subscription to a single topic on a pool-backed pub/sub region.
///
/// Returns [`SampleGuard`] references that implement `Deref<Target=[u8]>`.
/// These guards are **safe** -- the block is held alive by atomic refcounting
/// until the guard is dropped.
pub struct Subscription {
    pub(crate) region: Arc<Region>,
    pub(crate) topic_idx: u32,
    pub(crate) last_seq: Cell<u64>,
    pub(crate) type_size: u32,
}

impl Subscription {
    fn write_seq_atom(&self) -> &AtomicU64 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.base.add(off + TE_WRITE_SEQ) as *const AtomicU64) }
    }

    fn notify_atom(&self) -> &AtomicU32 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.base.add(off + TE_NOTIFY) as *const AtomicU32) }
    }

    fn waiters_atom(&self) -> &AtomicU32 {
        let off = topic_entry_off(self.topic_idx);
        unsafe { &*(self.region.base.add(off + TE_WAITERS) as *const AtomicU32) }
    }

    /// Non-blocking: returns the next sample guard or `None`.
    ///
    /// The returned guard implements `Deref<Target=[u8]>` -- safe to read
    /// without `unsafe`. The block is held alive until the guard is dropped.
    #[inline]
    pub fn try_recv(&self) -> Option<SampleGuard<'_>> {
        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq.get() {
            return None;
        }

        let target = if current_seq - self.last_seq.get() > self.region.config.ring_depth as u64 {
            current_seq // fell behind -- skip to latest
        } else {
            self.last_seq.get() + 1
        };

        if let Some(guard) = self.try_read_slot(target) {
            return Some(guard);
        }

        // Slot was overwritten -- retry with latest
        let latest = self.write_seq_atom().load(Ordering::Acquire);
        if latest > self.last_seq.get() && latest != target {
            self.try_read_slot(latest)
        } else {
            None
        }
    }

    /// Raw slot read: performs seqlock + refcount CAS and returns slot metadata.
    /// Shared by both untyped and typed recv paths.
    #[inline]
    fn try_read_slot_raw(&self, seq: u64) -> Option<SlotRead> {
        let ring_depth = self.region.config.ring_depth;
        let slot = (seq % ring_depth as u64) as u32;
        let entry_off = ring_entry_off(&self.region.config, self.topic_idx, slot);
        let entry_ptr = unsafe { self.region.base.add(entry_off) };

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

        // Bounds check
        if data_len as usize > self.region.data_capacity() {
            return None;
        }

        // CAS increment refcount -- acquire a reference to the block
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

        // Seqlock check 2 -- verify ring slot wasn't overwritten during our read
        if entry_seq.load(Ordering::Acquire) != seq {
            // Undo refcount increment
            let prev = refcount.fetch_sub(1, Ordering::AcqRel);
            if prev == 1 {
                self.region.free_block(block_idx);
            }
            return None;
        }

        self.last_seq.set(seq);

        Some(SlotRead {
            block_idx,
            data_len,
        })
    }

    #[inline]
    fn try_read_slot(&self, seq: u64) -> Option<SampleGuard<'_>> {
        let slot = self.try_read_slot_raw(seq)?;
        Some(SampleGuard {
            region: &self.region,
            block_idx: slot.block_idx,
            len: slot.data_len as usize,
        })
    }

    /// Compute the target seq for the next receive, or None if no new data.
    #[inline]
    fn next_target_seq(&self) -> Option<u64> {
        let current_seq = self.write_seq_atom().load(Ordering::Acquire);
        if current_seq <= self.last_seq.get() {
            return None;
        }

        Some(
            if current_seq - self.last_seq.get() > self.region.config.ring_depth as u64 {
                current_seq // fell behind -- skip to latest
            } else {
                self.last_seq.get() + 1
            },
        )
    }

    /// Non-blocking typed receive. Returns a [`TypedSampleGuard`] that
    /// dereferences to `&T`.
    ///
    /// # Panics
    ///
    /// Panics if the topic was registered with a non-zero type size that
    /// doesn't match `size_of::<T>()`.
    pub fn try_recv_typed<T: crate::Pod>(&self) -> Option<TypedSampleGuard<'_, T>> {
        if self.type_size != 0 {
            assert_eq!(
                self.type_size as usize,
                core::mem::size_of::<T>(),
                "type size mismatch: topic has {} bytes, expected {}",
                self.type_size,
                core::mem::size_of::<T>()
            );
        }

        let target = self.next_target_seq()?;

        if let Some(slot) = self.try_read_slot_raw(target) {
            return Some(TypedSampleGuard {
                region: &self.region,
                block_idx: slot.block_idx,
                _marker: core::marker::PhantomData,
            });
        }

        // Slot was overwritten -- retry with latest
        let latest = self.write_seq_atom().load(Ordering::Acquire);
        if latest > self.last_seq.get() && latest != target {
            self.try_read_slot_raw(latest).map(|slot| TypedSampleGuard {
                region: &self.region,
                block_idx: slot.block_idx,
                _marker: core::marker::PhantomData,
            })
        } else {
            None
        }
    }

    /// Blocking typed receive with the default [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_typed<T: crate::Pod>(
        &self,
    ) -> Result<TypedSampleGuard<'_, T>, crate::error::IpcError> {
        self.recv_typed_with::<T>(crate::WaitStrategy::default())
    }

    /// Blocking typed receive with a custom [`WaitStrategy`].
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_typed_with<T: crate::Pod>(
        &self,
        strategy: crate::WaitStrategy,
    ) -> Result<TypedSampleGuard<'_, T>, crate::error::IpcError> {
        // Fast path
        if let Some(g) = self.try_recv_typed::<T>() {
            return Ok(g);
        }

        let poll_ms = (self.region.config.stale_timeout.as_millis() as u64 / 3).clamp(1, 50);
        let mut iter: u32 = 0;

        loop {
            if let Some(g) = self.try_recv_typed::<T>() {
                return Ok(g);
            }

            match strategy {
                WaitStrategy::Adaptive {
                    spin_iters,
                    yield_iters,
                } if iter >= spin_iters + yield_iters => {
                    let cur = self.notify_atom().load(Ordering::Acquire);
                    if let Some(g) = self.try_recv_typed::<T>() {
                        return Ok(g);
                    }
                    self.waiters_atom().fetch_add(1, Ordering::Release);
                    let result = notify::wait_until_not(
                        self.notify_atom(),
                        cur,
                        Duration::from_millis(poll_ms),
                    );
                    self.waiters_atom().fetch_sub(1, Ordering::Release);

                    if result.is_err() {
                        self.region.check_heartbeat()?;
                    }
                }
                _ => {
                    strategy.wait(iter);
                    iter = iter.saturating_add(1);

                    if iter.is_multiple_of(1024) {
                        self.region.check_heartbeat()?;
                    }
                }
            }
        }
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
    /// Returns [`IpcError::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv_with(&self, strategy: WaitStrategy) -> Result<SampleGuard<'_>, IpcError> {
        // Fast path: check immediately
        if let Some(g) = self.try_recv() {
            return Ok(g);
        }

        let poll_ms = (self.region.config.stale_timeout.as_millis() as u64 / 3).clamp(1, 50);
        let mut iter: u32 = 0;

        loop {
            if let Some(g) = self.try_recv() {
                return Ok(g);
            }

            match strategy {
                WaitStrategy::Adaptive {
                    spin_iters,
                    yield_iters,
                } if iter >= spin_iters + yield_iters => {
                    // Phase 3: OS sleep (existing futex/WaitOnAddress/WFE path)
                    let cur = self.notify_atom().load(Ordering::Acquire);
                    if let Some(g) = self.try_recv() {
                        return Ok(g);
                    }
                    self.waiters_atom().fetch_add(1, Ordering::Release);
                    let result = notify::wait_until_not(
                        self.notify_atom(),
                        cur,
                        Duration::from_millis(poll_ms),
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

    /// Blocking: waits for the next sample using the default [`WaitStrategy`]
    /// (three-phase adaptive: spin -> yield -> OS sleep).
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::PublisherDead`] if the publisher heartbeat goes stale.
    pub fn recv(&self) -> Result<SampleGuard<'_>, IpcError> {
        self.recv_with(WaitStrategy::default())
    }
}

/// Raw slot data returned by `try_read_slot_raw`.
struct SlotRead {
    block_idx: u32,
    data_len: u32,
}

// ---- SampleGuard ----

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
///    inside an `Arc<Region>` held by the `Subscription`).
/// 2. The block cannot be freed while refcount > 0.
/// 3. No writer touches the block's data region after publishing.
/// 4. The data region does not overlap the free-list link field.
pub struct SampleGuard<'a> {
    region: &'a Region,
    block_idx: u32,
    len: usize,
}

impl SampleGuard<'_> {
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

impl Deref for SampleGuard<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            core::slice::from_raw_parts(ptr, self.len)
        }
    }
}

impl AsRef<[u8]> for SampleGuard<'_> {
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

impl std::fmt::Debug for SampleGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleGuard")
            .field("block_idx", &self.block_idx)
            .field("len", &self.len)
            .finish()
    }
}

impl Drop for SampleGuard<'_> {
    fn drop(&mut self) {
        let refcount = self.region.block_refcount(self.block_idx);
        let prev = refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.region.free_block(self.block_idx);
        }
    }
}

// ---- TypedSampleGuard ----

/// Typed zero-copy reference to a `T: Pod` value in shared memory.
///
/// Implements `Deref<Target=T>` -- safe to read without `unsafe`.
/// The underlying pool block is held alive by an atomic refcount and freed
/// back to the pool when this guard is dropped.
pub struct TypedSampleGuard<'a, T: crate::Pod> {
    region: &'a Region,
    block_idx: u32,
    _marker: core::marker::PhantomData<&'a T>,
}

impl<T: crate::Pod> core::ops::Deref for TypedSampleGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            &*(ptr as *const T)
        }
    }
}

impl<T: crate::Pod> Drop for TypedSampleGuard<'_, T> {
    fn drop(&mut self) {
        let refcount = self.region.block_refcount(self.block_idx);
        let prev = refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.region.free_block(self.block_idx);
        }
    }
}

impl<T: crate::Pod> core::fmt::Debug for TypedSampleGuard<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TypedSampleGuard")
            .field("block_idx", &self.block_idx)
            .field("size", &core::mem::size_of::<T>())
            .finish()
    }
}
