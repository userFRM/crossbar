// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Loan, TypedLoan, Topic.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;

use crate::error::Error;
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

// ---- Topic ----

/// Handle returned by [`super::shm::Publisher::register`]. Identifies a topic
/// for use with [`super::shm::Publisher::loan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Topic {
    pub(crate) topic_idx: u32,
    pub(crate) publisher_id: u64,
}

// ---- Loan ----

/// A mutable view into a pool block in shared memory.
///
/// Write your data (any format), then call [`publish`](Self::publish) to
/// transfer ownership to subscribers. The transfer is O(1) -- only 8 bytes
/// (block index + data length) are written to the ring, regardless of how
/// much data you wrote into the block.
///
/// If dropped without publishing, the block is freed back to the pool.
pub struct Loan<'a> {
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

impl<'a> Loan<'a> {
    /// Returns the writable data region as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data_ptr, self.capacity) }
    }

    /// Copies `data` into the block starting at offset 0.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DataTooLarge`] if `data` exceeds block data capacity.
    pub fn set_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > self.capacity {
            return Err(Error::DataTooLarge {
                size: data.len(),
                capacity: self.capacity,
            });
        }
        #[cfg(target_arch = "x86_64")]
        {
            if data.len() >= 2_097_152 {
                // Non-temporal stores bypass the cache hierarchy, avoiding
                // pollution for large payloads that subscribers will read
                // from their own cache lines.
                unsafe { nontemporal_copy(data.as_ptr(), self.data_ptr, data.len()) };
                self.len = data.len();
                return Ok(());
            }
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr, data.len());
        }
        self.len = data.len();
        Ok(())
    }

    /// Sets the valid data length (use after writing via `as_mut_slice`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::DataTooLarge`] if `len` exceeds block data capacity.
    pub fn set_len(&mut self, len: usize) -> Result<(), Error> {
        if len > self.capacity {
            return Err(Error::DataTooLarge {
                size: len,
                capacity: self.capacity,
            });
        }
        self.len = len;
        Ok(())
    }

    /// Maximum bytes this loan can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Write a `T: Pod` header followed by a `[E: Pod]` array into the block.
    ///
    /// Layout in the block: `[T bytes][E bytes * entries.len()][u32 entry_count]`
    /// The entry count is stored at the END so the reader can verify.
    ///
    /// # Errors
    ///
    /// Returns `Error::DataTooLarge` if header + array + 4 bytes exceeds capacity.
    pub fn write_structured<T: crate::Pod, E: crate::Pod>(
        &mut self,
        header: &T,
        entries: &[E],
    ) -> Result<(), Error> {
        let header_size = core::mem::size_of::<T>();
        let array_size = core::mem::size_of_val(entries);
        let total = header_size + array_size + 4; // +4 for entry count u32

        if total > self.capacity {
            return Err(Error::DataTooLarge {
                size: total,
                capacity: self.capacity,
            });
        }

        let buf = self.as_mut_slice();
        // Write header
        unsafe {
            core::ptr::copy_nonoverlapping(
                header as *const T as *const u8,
                buf.as_mut_ptr(),
                header_size,
            );
        }
        // Write array
        if !entries.is_empty() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    entries.as_ptr() as *const u8,
                    buf.as_mut_ptr().add(header_size),
                    array_size,
                );
            }
        }
        // Write entry count at the end
        let count = entries.len() as u32;
        buf[header_size + array_size..header_size + array_size + 4]
            .copy_from_slice(&count.to_le_bytes());

        self.set_len(total)?;
        Ok(())
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
        // Runtime check: len must fit in u32 (structurally enforced by u32 block_size,
        // but defense-in-depth against corruption via io::Write accumulation)
        assert!(self.len <= u32::MAX as usize, "data_len overflow");
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

impl<'a> Drop for Loan<'a> {
    fn drop(&mut self) {
        // Loan dropped without publish -- return block to pool
        self.region.free_block(self.block_idx);
    }
}

impl<'a> io::Write for Loan<'a> {
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

// ---- TypedLoan ----

/// Typed mutable loan of a `T: Pod` value in shared memory.
///
/// Use [`send`](Self::send) to write a value and publish in one step,
/// or [`as_mut`](Self::as_mut) + [`publish`](Self::publish) to fill
/// fields individually (born-in-SHM pattern).
///
/// If dropped without publishing, the block is freed back to the pool.
pub struct TypedLoan<'a, T: crate::Pod> {
    pub(crate) region: &'a Arc<Region>,
    pub(crate) block_idx: u32,
    pub(crate) topic_idx: u32,
    pub(crate) write_seq_atom: &'a AtomicU64,
    pub(crate) waiters_atom: &'a AtomicU32,
    pub(crate) single_publisher: bool,
    pub(crate) _marker: core::marker::PhantomData<&'a mut T>,
}

impl<'a, T: crate::Pod> TypedLoan<'a, T> {
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

impl<T: crate::Pod> Drop for TypedLoan<'_, T> {
    fn drop(&mut self) {
        // Loan dropped without publish -- return block to pool
        self.region.free_block(self.block_idx);
    }
}

// ---- PinnedLoan ----

/// Mutable view into a pinned block in shared memory.
///
/// The block is permanently assigned to the topic -- no allocation on loan,
/// no refcount. `publish()` is a single atomic Release store.
///
/// # Panics
///
/// `loan_pinned` returns an error if subscribers hold a `PinnedGuard`.
/// Overlapping reads and writes are prevented at runtime.
pub struct PinnedLoan<'a> {
    pub(crate) region: &'a Arc<Region>,
    pub(crate) data_ptr: *mut u8,
    pub(crate) capacity: usize,
    pub(crate) len: usize,
    pub(crate) topic_idx: u32,
    pub(crate) write_seq_atom: &'a AtomicU64,
    pub(crate) waiters_atom: &'a AtomicU32,
    pub(crate) readers: &'a AtomicU32,
}

impl<'a> PinnedLoan<'a> {
    /// Returns the writable data region as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data_ptr, self.capacity) }
    }

    /// Copies `data` into the block starting at offset 0.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DataTooLarge`] if `data` exceeds block data capacity.
    pub fn set_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > self.capacity {
            return Err(Error::DataTooLarge {
                size: data.len(),
                capacity: self.capacity,
            });
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr, data.len());
        }
        self.len = data.len();
        Ok(())
    }

    /// Sets the valid data length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DataTooLarge`] if `len` exceeds block data capacity.
    pub fn set_len(&mut self, len: usize) -> Result<(), Error> {
        if len > self.capacity {
            return Err(Error::DataTooLarge {
                size: len,
                capacity: self.capacity,
            });
        }
        self.len = len;
        Ok(())
    }

    /// Maximum bytes this loan can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Write a `T: Pod` header followed by a `[E: Pod]` array into the block.
    ///
    /// Layout in the block: `[T bytes][E bytes * entries.len()][u32 entry_count]`
    /// The entry count is stored at the END so the reader can verify.
    ///
    /// # Errors
    ///
    /// Returns `Error::DataTooLarge` if header + array + 4 bytes exceeds capacity.
    pub fn write_structured<T: crate::Pod, E: crate::Pod>(
        &mut self,
        header: &T,
        entries: &[E],
    ) -> Result<(), Error> {
        let header_size = core::mem::size_of::<T>();
        let array_size = core::mem::size_of_val(entries);
        let total = header_size + array_size + 4; // +4 for entry count u32

        if total > self.capacity {
            return Err(Error::DataTooLarge {
                size: total,
                capacity: self.capacity,
            });
        }

        let buf = self.as_mut_slice();
        // Write header
        unsafe {
            core::ptr::copy_nonoverlapping(
                header as *const T as *const u8,
                buf.as_mut_ptr(),
                header_size,
            );
        }
        // Write array
        if !entries.is_empty() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    entries.as_ptr() as *const u8,
                    buf.as_mut_ptr().add(header_size),
                    array_size,
                );
            }
        }
        // Write entry count at the end
        let count = entries.len() as u32;
        buf[header_size + array_size..header_size + array_size + 4]
            .copy_from_slice(&count.to_le_bytes());

        self.set_len(total)?;
        Ok(())
    }

    /// Publish. 1 atomic Release store + clear writer sentinel.
    #[inline]
    pub fn publish(self) {
        assert!(self.len <= u32::MAX as usize, "data_len overflow");
        self.region.commit_pinned(
            self.len as u32,
            self.topic_idx,
            self.write_seq_atom,
            self.waiters_atom,
        );
        // Clear the writer sentinel so subscribers can enter again
        self.readers.store(0, core::sync::atomic::Ordering::Release);
        // Skip Drop — we already cleared the sentinel
        core::mem::forget(self);
    }
}

impl Drop for PinnedLoan<'_> {
    fn drop(&mut self) {
        // PinnedLoan dropped without publish — clear the writer sentinel
        // so subscribers aren't permanently blocked. Data in the pinned
        // block is stale (partial write), but the old pinned_seq is still
        // valid so subscribers won't see the partial data.
        self.readers.store(0, core::sync::atomic::Ordering::Release);
    }
}
