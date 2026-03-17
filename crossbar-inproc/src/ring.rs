// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lock-free SPSC ring buffer with lossy overflow.
//!
//! Each subscriber gets its own dedicated ring. The publisher iterates
//! subscribers and pushes `Arc::clone` into each ring. This is O(N) in
//! subscribers but each push is ~19ns (vs ~70ns for ArcSwap::store).

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cache-line aligned wrapper to prevent false sharing.
#[repr(align(64))]
pub(crate) struct CacheAligned<T>(pub T);

/// Lock-free SPSC ring with lossy overflow and cache-line padded indices.
///
/// Single producer writes via `push`, single consumer reads via `pop`.
/// When full, the producer CAS-advances the tail, dropping the oldest value.
pub(crate) struct Ring<T> {
    slots: Box<[UnsafeCell<MaybeUninit<Arc<T>>>]>,
    head: CacheAligned<AtomicU64>,
    tail: CacheAligned<AtomicU64>,
    capacity: u64,
    drop_count: AtomicU64,
}

// Safety: Ring<T> is Send if T: Send. The SPSC contract means only one thread
// writes (push) and one thread reads (pop), but both ends may live on
// different threads.
unsafe impl<T: Send> Send for Ring<T> {}
// Safety: Ring<T> is Sync if T: Send + Sync. The atomic head/tail provide
// the necessary synchronisation between producer and consumer threads.
unsafe impl<T: Send + Sync> Sync for Ring<T> {}

impl<T> Ring<T> {
    /// Create a new SPSC ring with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring capacity must be > 0");
        let slots: Vec<UnsafeCell<MaybeUninit<Arc<T>>>> = (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect();
        Self {
            slots: slots.into_boxed_slice(),
            head: CacheAligned(AtomicU64::new(0)),
            tail: CacheAligned(AtomicU64::new(0)),
            capacity: capacity as u64,
            drop_count: AtomicU64::new(0),
        }
    }

    /// Push a value into the ring (producer side).
    ///
    /// If the ring is full, CAS-advances the tail to drop the oldest value.
    #[inline]
    pub(crate) fn push(&self, value: Arc<T>) {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);

        // If full, advance tail (drop oldest)
        if head - tail >= self.capacity {
            let new_tail = tail + 1;
            // CAS to advance tail — if consumer already advanced it, that's fine.
            if self
                .tail
                .0
                .compare_exchange(tail, new_tail, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // We won the CAS — drop the old value at tail slot.
                let idx = (tail % self.capacity) as usize;
                unsafe {
                    let slot = &mut *self.slots[idx].get();
                    slot.assume_init_drop();
                }
                self.drop_count.fetch_add(1, Ordering::Relaxed);
            }
            // If CAS failed, consumer already advanced tail — ring has space now.
        }

        // Write value into head slot
        let idx = (head % self.capacity) as usize;
        unsafe {
            let slot = &mut *self.slots[idx].get();
            slot.write(value);
        }

        // Advance head with Release so consumer sees the written value.
        self.head.0.store(head + 1, Ordering::Release);
    }

    /// Pop a value from the ring (consumer side).
    ///
    /// Returns `None` if the ring is empty.
    #[inline]
    pub(crate) fn pop(&self) -> Option<Arc<T>> {
        loop {
            let tail = self.tail.0.load(Ordering::Relaxed);
            let head = self.head.0.load(Ordering::Acquire);

            if tail >= head {
                return None; // empty
            }

            // CAS to advance tail — producer may also CAS tail on overflow.
            if self
                .tail
                .0
                .compare_exchange(tail, tail + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let idx = (tail % self.capacity) as usize;
                let value = unsafe {
                    let slot = &*self.slots[idx].get();
                    slot.assume_init_read()
                };
                return Some(value);
            }
            // CAS failed — another thread (producer overflow) moved tail. Retry.
        }
    }

    /// Approximate number of items in the ring.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        head.saturating_sub(tail) as usize
    }

    /// Total number of values dropped due to overflow.
    #[inline]
    pub(crate) fn drops(&self) -> u64 {
        self.drop_count.load(Ordering::Relaxed)
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        // Drop remaining values in the ring.
        let tail = *self.tail.0.get_mut();
        let head = *self.head.0.get_mut();
        for seq in tail..head {
            let idx = (seq % self.capacity) as usize;
            unsafe {
                let slot = self.slots[idx].get_mut();
                slot.assume_init_drop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_basic() {
        let ring = Ring::new(4);
        ring.push(Arc::new(10));
        ring.push(Arc::new(20));
        assert_eq!(*ring.pop().unwrap(), 10);
        assert_eq!(*ring.pop().unwrap(), 20);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn overflow_drops_oldest() {
        let ring = Ring::new(2);
        ring.push(Arc::new(1));
        ring.push(Arc::new(2));
        ring.push(Arc::new(3)); // drops 1
        assert_eq!(ring.drops(), 1);
        assert_eq!(*ring.pop().unwrap(), 2);
        assert_eq!(*ring.pop().unwrap(), 3);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn len_tracking() {
        let ring = Ring::new(4);
        assert_eq!(ring.len(), 0);
        ring.push(Arc::new(1));
        assert_eq!(ring.len(), 1);
        ring.push(Arc::new(2));
        assert_eq!(ring.len(), 2);
        ring.pop();
        assert_eq!(ring.len(), 1);
    }

    #[test]
    #[should_panic(expected = "ring capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ring = Ring::<u8>::new(0);
    }
}
