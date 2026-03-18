// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! ShmLoan, TypedShmLoan, TopicHandle.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;

use crate::protocol::layout::BLOCK_DATA_OFFSET;
use crate::protocol::Region;

// ---- Non-temporal copy (x86-64) ----

/// Copies `len` bytes from `src` to `dst` using non-temporal (streaming) stores.
/// The 16-byte-aligned portion uses `_mm_stream_si128`; any head/tail remainder
/// uses `copy_nonoverlapping`. An `_mm_sfence` at the end ensures all stores
/// are globally visible before the function returns.
///
/// # Safety
///
/// `src` and `dst` must be valid for `len` bytes and must not overlap.
#[cfg(target_arch = "x86_64")]
unsafe fn nontemporal_copy(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_sfence, _mm_stream_si128};

    let mut offset = 0usize;

    // Handle unaligned head: copy bytes until dst is 16-byte aligned
    let align_offset = dst.align_offset(16).min(len);
    if align_offset > 0 {
        core::ptr::copy_nonoverlapping(src, dst, align_offset);
        offset = align_offset;
    }

    // Stream 16 bytes at a time via non-temporal stores
    while offset + 16 <= len {
        let chunk = _mm_loadu_si128(src.add(offset) as *const __m128i);
        _mm_stream_si128(dst.add(offset) as *mut __m128i, chunk);
        offset += 16;
    }

    // Handle remainder tail
    if offset < len {
        core::ptr::copy_nonoverlapping(src.add(offset), dst.add(offset), len - offset);
    }

    // Ensure all streaming stores are visible before any subsequent loads
    _mm_sfence();
}

// ---- TopicHandle ----

/// Handle returned by [`super::shm::ShmPublisher::register`]. Identifies a topic
/// for use with [`super::shm::ShmPublisher::loan`].
#[derive(Clone)]
pub struct TopicHandle {
    pub(crate) topic_idx: u32,
    pub(crate) publisher_id: u64,
}

// ---- ShmLoan ----

/// A mutable view into a pool block in shared memory.
///
/// Write your data (any format), then call [`publish`](Self::publish) to
/// transfer ownership to subscribers. The transfer is O(1) -- only 8 bytes
/// (block index + data length) are written to the ring, regardless of how
/// much data you wrote into the block.
///
/// If dropped without publishing, the block is freed back to the pool.
pub struct ShmLoan<'a> {
    pub(crate) region: &'a Arc<Region>,
    pub(crate) data_ptr: *mut u8,
    pub(crate) capacity: usize,
    pub(crate) len: usize,
    pub(crate) block_idx: u32,
    pub(crate) topic_idx: u32,
    pub(crate) write_seq_atom: &'a AtomicU64,
    pub(crate) waiters_atom: &'a AtomicU32,
    pub(crate) single_publisher: bool,
}

impl<'a> ShmLoan<'a> {
    /// Returns the writable data region as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data_ptr, self.capacity) }
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
        #[cfg(target_arch = "x86_64")]
        {
            if data.len() >= 2_097_152 {
                // Non-temporal stores bypass the cache hierarchy, avoiding
                // pollution for large payloads that subscribers will read
                // from their own cache lines.
                unsafe { nontemporal_copy(data.as_ptr(), self.data_ptr, data.len()) };
                self.len = data.len();
                return;
            }
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr, data.len());
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
    /// O(1) -- writes 8 bytes to the ring regardless of payload size.
    #[inline]
    pub fn publish(self) {
        self.commit(true);
        core::mem::forget(self); // block is published; don't run Drop
    }

    /// Publishes without waking subscribers. Saves ~170 ns by skipping
    /// the futex syscall.
    #[inline]
    pub fn publish_silent(self) {
        self.commit(false);
        core::mem::forget(self);
    }

    #[inline]
    fn commit(&self, wake: bool) {
        self.region.commit_to_ring(
            self.block_idx,
            self.len as u32,
            self.topic_idx,
            self.write_seq_atom,
            self.waiters_atom,
            wake,
            self.single_publisher,
        );
    }
}

impl<'a> Drop for ShmLoan<'a> {
    fn drop(&mut self) {
        // Loan dropped without publish -- return block to pool
        self.region.free_block(self.block_idx);
    }
}

impl<'a> io::Write for ShmLoan<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.capacity - self.len;
        if remaining == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "block full"));
        }
        let n = buf.len().min(remaining);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), self.data_ptr.add(self.len), n);
        }
        self.len += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---- TypedShmLoan ----

/// Typed mutable loan of a `T: Pod` value in shared memory.
///
/// Use [`send`](Self::send) to write a value and publish in one step,
/// or [`as_mut`](Self::as_mut) + [`publish`](Self::publish) to fill
/// fields individually (born-in-SHM pattern).
///
/// If dropped without publishing, the block is freed back to the pool.
pub struct TypedShmLoan<'a, T: crate::Pod> {
    pub(crate) region: &'a Arc<Region>,
    pub(crate) block_idx: u32,
    pub(crate) topic_idx: u32,
    pub(crate) write_seq_atom: &'a AtomicU64,
    pub(crate) waiters_atom: &'a AtomicU32,
    pub(crate) single_publisher: bool,
    pub(crate) _marker: core::marker::PhantomData<&'a mut T>,
}

impl<'a, T: crate::Pod> TypedShmLoan<'a, T> {
    /// Returns a mutable reference to the `T` value in shared memory.
    ///
    /// Use this for the born-in-SHM pattern: fill fields individually,
    /// then call [`publish`](Self::publish).
    #[allow(clippy::should_implement_trait)]
    pub fn as_mut(&mut self) -> &mut T {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            &mut *(ptr as *mut T)
        }
    }

    /// Writes `value` into the shared-memory block and publishes it,
    /// waking any blocked subscribers.
    #[inline]
    pub fn send(self, value: T) {
        unsafe {
            let ptr = self.region.block_ptr(self.block_idx).add(BLOCK_DATA_OFFSET);
            core::ptr::write(ptr as *mut T, value);
        }
        self.commit(true);
        core::mem::forget(self);
    }

    /// Publishes whatever was written via [`as_mut`](Self::as_mut),
    /// waking any blocked subscribers.
    #[inline]
    pub fn publish(self) {
        self.commit(true);
        core::mem::forget(self);
    }

    /// Publishes without waking subscribers.
    #[inline]
    pub fn publish_silent(self) {
        self.commit(false);
        core::mem::forget(self);
    }

    #[inline]
    fn commit(&self, wake: bool) {
        self.region.commit_to_ring(
            self.block_idx,
            core::mem::size_of::<T>() as u32,
            self.topic_idx,
            self.write_seq_atom,
            self.waiters_atom,
            wake,
            self.single_publisher,
        );
    }
}

impl<T: crate::Pod> Drop for TypedShmLoan<'_, T> {
    fn drop(&mut self) {
        // Loan dropped without publish -- return block to pool
        self.region.free_block(self.block_idx);
    }
}
